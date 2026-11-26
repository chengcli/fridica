//! Host-owned native MCP binding. No secrets or project protocol in configuration.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const NATIVE_ENV: &str = "OVERLEAF_NATIVE_SESSION";
pub const NATIVE_ENTRY: &str = "fridica/overleaf/native-session";
pub const OPERATIONS: [&str; 7] = [
    "list_comments",
    "read_comment_thread",
    "add_comment",
    "reply_comment",
    "read_file",
    "list_tracked_changes",
    "find_and_replace",
];
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Installed thin adapter argv, e.g. ["python3", "/.../adapter.py", "--node", "/.../node"].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub native_adapter: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_recipient: Option<Recipient>,
    /// Verified account and optional legacy limits; absent scope denies native forwarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Login {
    pub resources: Option<std::path::PathBuf>,
    pub node: Option<std::path::PathBuf>,
    pub browser: Option<std::path::PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub expected_email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}
impl Scope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.project.as_deref().is_none_or(valid_id) && valid_email(&self.expected_email),
            "native login requires an account email and, if restricted, a 24-character project ID"
        );
        ensure!(
            self.channel
                .as_deref()
                .is_none_or(|c| super::loader::slack_id(c, "CG")),
            "native channel must be an explicit Slack channel ID"
        );
        Ok(())
    }
}
fn valid_id(id: &str) -> bool {
    id.len() == 24
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_email(email: &str) -> bool {
    !email.is_empty()
        && email.len() <= 254
        && email.is_ascii()
        && email.split('@').count() == 2
        && email.split('@').all(|v| !v.is_empty())
        && !email.bytes().any(|b| b <= 32 || b == 127)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub worker_id: String,
    pub machine: String,
    pub workspace: String,
    /// Set only for an explicitly timed owner grant; otherwise actual session expiry/revocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}
impl Recipient {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.worker_id
                .strip_prefix("worker-")
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .is_some(),
            "recipient requires a worker UUID"
        );
        ensure!(
            [&self.machine, &self.workspace]
                .iter()
                .all(|s| !s.is_empty() && !s.chars().any(char::is_control)),
            "native recipient requires an explicit registered machine and workspace"
        );
        ensure!(
            self.expires_at != Some(0),
            "invalid native recipient expiry"
        );
        Ok(())
    }
    pub fn permits(&self, worker: &str, machine: &str, workspace: &str) -> bool {
        self.validate().is_ok()
            && self.worker_id == worker
            && self.machine == machine
            && self.workspace == workspace
            && self.expires_at.is_none_or(|t| t > now())
    }
}
pub fn validate(config: &super::Config) -> Result<()> {
    ensure!(
        config
            .overleaf
            .native_adapter
            .iter()
            .all(|s| !s.is_empty() && !s.chars().any(char::is_control)),
        "native adapter argv contains an empty/control argument"
    );
    if let Some(scope) = &config.overleaf.native_scope {
        scope.validate()?;
        ensure!(
            scope
                .channel
                .as_ref()
                .is_none_or(|c| config.slack.channels.contains(c)),
            "native channel must be in the owner Slack allowlist"
        );
    }
    if let Some(r) = &config.overleaf.native_recipient {
        r.validate()?;
        ensure!(
            config.overleaf.enabled
                && config
                    .machines
                    .get(&r.machine)
                    .and_then(|m| m.workspace(&r.workspace))
                    .is_some(),
            "native recipient requires an enabled registered placement"
        );
    }
    Ok(())
}

/// Credential envelope only; Overleaf's protocol remains in the separate AGPL subprocess.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    schema: u8,
    cookie: String,
    account_id: String,
    expected_email: String,
    projects: Option<Vec<String>>,
    operations: Vec<String>,
    expires_at: Option<u64>,
    recipient: Option<Recipient>,
    channel: Option<String>,
}
pub(crate) fn validate_session(raw: &str) -> Result<()> {
    ensure!(raw.len() <= 16384, "native session too large");
    let s: Session =
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid native session"))?;
    ensure!(
        matches!(s.schema, 2 | 3)
            && (s.schema != 2 || s.projects.as_ref().is_some_and(|p| p.len() == 1))
            && s.projects
                .as_ref()
                .is_none_or(|p| !p.is_empty() && p.iter().all(|id| valid_id(id)))
            && s.operations == OPERATIONS
            && valid_id(&s.account_id)
            && valid_email(&s.expected_email)
            && s.expires_at.is_none_or(|t| t > now()),
        "native session outside scope or expired"
    );
    let mut names = std::collections::BTreeSet::new();
    for pair in s.cookie.split("; ") {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid native cookie"))?;
        ensure!(
            matches!(name, "overleaf_session2" | "GCLB")
                && names.insert(name)
                && !value.is_empty()
                && value.len() <= 4096
                && value
                    .bytes()
                    .all(|b| (33..127).contains(&b) && !b";\"\\,".contains(&b)),
            "invalid native cookie"
        );
    }
    ensure!(
        names.contains("overleaf_session2"),
        "native session cookie missing"
    );
    if let Some(r) = &s.recipient {
        r.validate()?;
    }
    ensure!(
        s.channel
            .as_deref()
            .is_none_or(|c| super::loader::slack_id(c, "CG")),
        "native channel mismatch"
    );
    Ok(())
}
pub(crate) fn validate_session_scope(raw: &str, scope: &Scope) -> Result<()> {
    validate_session(raw)?;
    scope.validate()?;
    let s: Session = serde_json::from_str(raw)?;
    ensure!(
        s.expected_email == scope.expected_email
            && scope
                .project
                .as_ref()
                .is_none_or(|p| s.projects.as_ref().is_none_or(|ids| ids.contains(p)))
            && s.channel
                .as_ref()
                .is_none_or(|c| scope.channel.as_ref().is_none_or(|limit| c == limit)),
        "saved native session does not match configured account/project/channel"
    );
    Ok(())
}
/// A new account-wide login cannot erase a legacy grant stored with the session.
pub(crate) fn renew_session(previous: &[u8], raw: &str) -> Result<String> {
    validate_session(raw)?;
    let old: Session = serde_json::from_slice(previous)
        .map_err(|_| anyhow::anyhow!("invalid saved session; explicit revocation required"))?;
    let mut new: serde_json::Value = serde_json::from_str(raw)?;
    ensure!(
        old.account_id == new["account_id"] && old.expected_email == new["expected_email"],
        "native account changed; explicit revocation required"
    );
    if let Some(projects) = old.projects {
        let permitted: Vec<_> = projects
            .into_iter()
            .filter(|p| {
                new["projects"]
                    .as_array()
                    .is_none_or(|ids| ids.iter().any(|id| id == p))
            })
            .collect();
        new["projects"] = serde_json::json!(permitted);
    } else {
        ensure!(
            old.schema == 3,
            "invalid legacy grant; explicit revocation required"
        );
    }
    if let Some(channel) = old.channel {
        ensure!(
            new["channel"].is_null() || new["channel"] == channel,
            "native channel changed; explicit revocation required"
        );
        new["channel"] = channel.into();
    }
    if let Some(recipient) = old.recipient {
        let saved = serde_json::to_value(recipient)?;
        ensure!(
            new["recipient"].is_null() || new["recipient"] == saved,
            "native recipient changed; explicit revocation required"
        );
        new["recipient"] = saved;
    }
    if let Some(deadline) = old.expires_at {
        new["expires_at"] = serde_json::json!(new["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    let renewed = new.to_string();
    validate_session(&renewed)?;
    Ok(renewed)
}

pub(crate) fn bound_session(
    raw: &str,
    recipient: &Recipient,
    configured_recipient: Option<&Recipient>,
    scope: &Scope,
    channel: &str,
) -> Result<String> {
    validate_session_scope(raw, scope)?;
    recipient.validate()?;
    let saved: Session = serde_json::from_str(raw)?;
    // Schema2 predates automatic admission. An absent legacy grant was a denial,
    // not permission to bind the saved login to any newly admitted worker.
    ensure!(
        saved.schema == 3
            || (scope.channel.is_some()
                && configured_recipient.is_some_and(|r| r.permits(
                    &recipient.worker_id,
                    &recipient.machine,
                    &recipient.workspace
                ))),
        "legacy native session requires configured recipient and channel grants"
    );
    ensure!(
        super::loader::slack_id(channel, "CG")
            && scope.channel.as_deref().is_none_or(|c| c == channel)
            && saved.channel.as_deref().is_none_or(|c| c == channel)
            && saved.recipient.as_ref().is_none_or(|r| r.permits(
                &recipient.worker_id,
                &recipient.machine,
                &recipient.workspace
            )),
        "saved native session does not permit this recipient/channel"
    );
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    if let Some(project) = &scope.project {
        value["projects"] = serde_json::json!([project]);
    }
    for deadline in [
        recipient.expires_at,
        saved.recipient.and_then(|r| r.expires_at),
    ]
    .into_iter()
    .flatten()
    {
        value["expires_at"] = serde_json::json!(value["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    value["recipient"] = serde_json::to_value(recipient)?;
    value["channel"] = channel.into();
    let raw = value.to_string();
    validate_session(&raw)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROJECT: &str = "333333333333333333333333";
    const CHANNEL: &str = "CEXAMPLE";
    #[test]
    fn grant_is_exact_and_lifetime_is_not_artificially_shortened() {
        let scope = Scope {
            expected_email: "owner@example.invalid".into(),
            project: Some(PROJECT.into()),
            channel: Some(CHANNEL.into()),
        };
        let r = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        let s = serde_json::json!({"schema":2,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":[PROJECT], "operations":OPERATIONS});
        let b: serde_json::Value = serde_json::from_str(
            &bound_session(&s.to_string(), &r, Some(&r), &scope, CHANNEL).unwrap(),
        )
        .unwrap();
        assert!(b["expires_at"].is_null());
        assert_eq!(b["channel"], CHANNEL);
        assert!(!r.permits(
            "worker-22222222-2222-4222-8222-222222222222",
            "dungeon3",
            "snapy"
        ));
        assert!(validate_session_scope(
            &s.to_string().replace(PROJECT, "222222222222222222222222"),
            &scope
        )
        .is_err());
        let mut timed = r;
        timed.expires_at = Some(now() + 60);
        let b: serde_json::Value = serde_json::from_str(
            &bound_session(&s.to_string(), &timed, Some(&timed), &scope, CHANNEL).unwrap(),
        )
        .unwrap();
        assert_eq!(b["expires_at"], timed.expires_at.unwrap());
    }
    #[test]
    fn account_session_retains_saved_and_configured_legacy_limits() {
        let scope = Scope {
            expected_email: "owner@example.invalid".into(),
            project: None,
            channel: None,
        };
        let recipient = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "test".into(),
            workspace: "paper".into(),
            expires_at: None,
        };
        let raw = serde_json::json!({"schema":3,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":scope.expected_email,"operations":OPERATIONS});
        let bound: serde_json::Value = serde_json::from_str(
            &bound_session(&raw.to_string(), &recipient, None, &scope, CHANNEL).unwrap(),
        )
        .unwrap();
        assert!(bound.get("projects").is_none());
        assert_eq!(bound["recipient"]["worker_id"], recipient.worker_id);
        let restricted = Scope {
            project: Some(PROJECT.into()),
            channel: Some(CHANNEL.into()),
            ..scope.clone()
        };
        let mut legacy: serde_json::Value = serde_json::from_str(
            &bound_session(&raw.to_string(), &recipient, None, &restricted, CHANNEL).unwrap(),
        )
        .unwrap();
        let deadline = now() + 3600;
        legacy["schema"] = 2.into();
        legacy["expires_at"] = deadline.into();
        legacy["recipient"]["expires_at"] = deadline.into();
        let legacy = legacy.to_string();
        let renewed = renew_session(legacy.as_bytes(), &raw.to_string()).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&renewed).unwrap();
        assert_eq!(saved["schema"], 3);
        assert_eq!(saved["projects"], serde_json::json!([PROJECT]));
        assert_eq!(saved["channel"], CHANNEL);
        assert_eq!(saved["recipient"]["worker_id"], recipient.worker_id);
        assert_eq!(saved["recipient"]["expires_at"], deadline);
        assert_eq!(saved["expires_at"], deadline);
        assert!(bound_session(&renewed, &recipient, None, &scope, "COTHER").is_err());
        let other = Recipient {
            worker_id: "worker-22222222-2222-4222-8222-222222222222".into(),
            ..recipient.clone()
        };
        assert!(bound_session(&renewed, &other, None, &scope, CHANNEL).is_err());
        assert!(renew_session(
            legacy.as_bytes(),
            &raw.to_string().replace("owner@", "other@")
        )
        .is_err());
        assert!(
            validate_session(&raw.to_string().replace("\"schema\":3", "\"schema\":2")).is_err()
        );
    }
}
