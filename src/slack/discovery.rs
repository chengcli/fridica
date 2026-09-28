//! Read-only onboarding, before a valid Config or state database exists.
//! The only network operations are auth.test and conversations.list.
use super::web::{self, Failure};
use crate::{config::loader, core::delivery::AdapterFuture};
use anyhow::{bail, Result};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Client, Url,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Identity,
    Channels { private: bool, cursor: String },
}
pub trait Api: Send + Sync {
    fn get(&self, request: Request) -> AdapterFuture<'_, std::result::Result<Value, Failure>>;
}
/// No Debug implementation: this adapter owns the user's credential.
pub struct Web {
    client: Client,
    authorization: HeaderValue,
    base: Url,
}
impl Web {
    #[cfg(test)]
    pub(crate) fn test_endpoint(&mut self, base: Url) {
        self.base = base;
    }
    pub fn new(token: &str) -> Result<Self> {
        if !token.starts_with("xoxp-")
            || token.len() < 6
            || token.bytes().any(|b| !b.is_ascii_graphic())
        {
            bail!("set the configured Slack user-token environment variable before detection");
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| anyhow::anyhow!("invalid Slack user credential"))?;
        authorization.set_sensitive(true);
        let builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10));
        #[cfg(test)]
        let builder = builder.no_proxy();
        let client = builder
            .build()
            .map_err(|_| anyhow::anyhow!("Slack discovery client unavailable"))?;
        Ok(Self {
            client,
            authorization,
            base: Url::parse("https://slack.com/api/")?,
        })
    }
}
impl Api for Web {
    fn get(&self, request: Request) -> AdapterFuture<'_, std::result::Result<Value, Failure>> {
        Box::pin(async move {
            let (method, body) = match request {
                Request::Identity => ("auth.test", json!({})),
                Request::Channels { private, cursor } => {
                    if cursor.len() > 4096 {
                        return Err(Failure::InvalidResponse);
                    }
                    (
                        "conversations.list",
                        json!({"types":if private {"private_channel"} else {"public_channel"},"exclude_archived":true,"limit":200,"cursor":cursor}),
                    )
                }
            };
            let url = self.base.join(method).map_err(|_| Failure::Configuration)?;
            // Match Slack's discovery protocol: channel listing is GET with
            // encoded query arguments; auth.test is POST.
            let request = if method == "auth.test" {
                self.client.post(url).json(&body)
            } else {
                self.client.get(url).query(&body)
            };
            let response =
                web::read(request.header(AUTHORIZATION, self.authorization.clone())).await?;
            let mut value = web::decode(&response)?;
            let token = self
                .authorization
                .to_str()
                .unwrap_or("")
                .strip_prefix("Bearer ")
                .unwrap_or("");
            web::redact(&mut value, token);
            Ok(value)
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Channel {
    pub id: String,
    pub name: String,
}
#[derive(Debug)]
pub struct Discovery {
    pub owner: String,
    pub workspace: String,
    pub channels: Vec<Channel>,
    pub warnings: Vec<String>,
}
pub async fn discover(api: &dyn Api) -> Result<Discovery> {
    tokio::time::timeout(Duration::from_secs(60), collect(api))
        .await
        .map_err(|_| anyhow::anyhow!("Slack discovery timed out; configuration unchanged"))?
}
async fn collect(api: &dyn Api) -> Result<Discovery> {
    let identity = api.get(Request::Identity).await?;
    let owner = identity["user_id"].as_str().unwrap_or("");
    let workspace = identity["team_id"].as_str().unwrap_or("");
    if !loader::slack_id(owner, "UW")
        || owner.len() > 64
        || !loader::slack_id(workspace, "T")
        || workspace.len() > 64
        || identity
            .get("bot_id")
            .is_some_and(|v| !v.is_null() && v != "")
    {
        bail!("Slack did not return a workspace user identity; use a user token");
    }
    let mut channels = BTreeMap::new();
    let mut warnings = vec![];
    for private in [false, true] {
        let mut cursor = String::new();
        let mut seen = BTreeSet::new();
        for page in 0..100 {
            let response = match api.get(Request::Channels { private, cursor }).await {
                Ok(response) => response,
                Err(Failure::Rejected { code }) if code == "missing_scope" => {
                    warnings.push(format!(
                        "Cannot discover {} channels: add {} as a user scope and reinstall.",
                        if private { "private" } else { "public" },
                        if private {
                            "groups:read"
                        } else {
                            "channels:read"
                        }
                    ));
                    break;
                }
                Err(error) => return Err(error.into()),
            };
            if let Some(rows) = response.get("channels") {
                for row in rows.as_array().ok_or(Failure::InvalidResponse)? {
                    let (Some(id), Some(name)) = (row["id"].as_str(), row["name"].as_str()) else {
                        continue;
                    };
                    if row["is_member"] == true
                        && row["is_archived"] != true
                        && row["is_im"] != true
                        && row["is_mpim"] != true
                        && loader::slack_id(id, "CG")
                        && id.len() <= 64
                        && !name.is_empty()
                        && name.len() <= 200
                    {
                        channels.insert(
                            id.to_string(),
                            Channel {
                                id: id.into(),
                                name: name.into(),
                            },
                        );
                    }
                    if channels.len() > 10000 {
                        bail!("Slack channel discovery exceeds its bound; configuration unchanged");
                    }
                }
            }
            cursor = match response
                .get("response_metadata")
                .and_then(|v| v.get("next_cursor"))
            {
                None => String::new(),
                Some(value) => value
                    .as_str()
                    .ok_or(Failure::InvalidResponse)?
                    .trim()
                    .to_string(),
            };
            if cursor.is_empty() {
                break;
            }
            if cursor.len() > 4096 || !seen.insert(cursor.clone()) {
                bail!("Slack repeated or invalid pagination cursor; configuration unchanged");
            }
            if page == 99 {
                bail!("Slack discovery exceeded its page bound; configuration unchanged");
            }
        }
    }
    let mut channels: Vec<_> = channels.into_values().collect();
    channels.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    Ok(Discovery {
        owner: owner.into(),
        workspace: workspace.into(),
        channels,
        warnings,
    })
}
pub fn select_names(channels: &[Channel], names: &[String]) -> Result<Vec<String>> {
    if channels.is_empty() {
        bail!("No joined channels found; check membership and channel read scopes");
    }
    let mut ids = vec![];
    for name in names {
        let name = name.strip_prefix('#').unwrap_or(name);
        let matching: Vec<_> = channels.iter().filter(|c| c.name == name).collect();
        if matching.len() != 1 {
            bail!(
                "A requested channel name was not found or is ambiguous; use interactive selection"
            );
        }
        if !ids.contains(&matching[0].id) {
            ids.push(matching[0].id.clone());
        }
    }
    if ids.is_empty() {
        bail!("Selection cancelled; configuration unchanged");
    }
    Ok(ids)
}
pub fn select_numbers(channels: &[Channel], answer: &str) -> Result<Vec<String>> {
    if answer.trim().is_empty() {
        bail!("Selection cancelled; configuration unchanged");
    }
    let mut ids = vec![];
    for number in answer.trim().split(',').map(str::trim) {
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            bail!("Invalid channel selection; configuration unchanged");
        }
        let n: usize = number
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid channel selection; configuration unchanged"))?;
        let channel = n
            .checked_sub(1)
            .and_then(|n| channels.get(n))
            .ok_or_else(|| anyhow::anyhow!("Invalid channel selection; configuration unchanged"))?;
        if !ids.contains(&channel.id) {
            ids.push(channel.id.clone());
        }
    }
    Ok(ids)
}
