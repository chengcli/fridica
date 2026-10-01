//! Fridica's Slack Web API: the fridica-slack client, journaled to the replay
//! ledger, and Fridica's delivery policy for outbox posts.
use super::{ingress::timestamp, journal::StoreJournal};
use crate::{
    config::Config,
    core::{
        delivery::{AdapterFuture, ClaimedPost, Delivery, DeliveryOutcome},
        ids::ThreadId,
        time::Clock,
    },
    store::Store,
};
pub use fridica_slack::web::{Failure, Identity, Post, Upload, WebClient};
use fridica_slack::{
    files::{Download, Downloader, Failure as FileFailure},
    history::{History, HistoryFailure, PageRequest},
    links::{Entry, Failure as LinkFailure, Link, Reader},
    BoxFuture, Scope,
};
use serde_json::{json, Value};
use std::{ops::Deref, sync::Arc, time::Duration};

/// The owner's Slack client as Fridica's adapter: delivery for the outbox,
/// history for catch-up, and links and files for parent context. A clone
/// shares the client's validation state.
#[derive(Clone)]
pub struct SlackClient(WebClient);
impl From<WebClient> for SlackClient {
    fn from(web: WebClient) -> Self {
        Self(web)
    }
}
impl Deref for SlackClient {
    type Target = WebClient;
    fn deref(&self) -> &WebClient {
        &self.0
    }
}
impl History for SlackClient {
    fn page(&self, request: PageRequest) -> BoxFuture<'_, Result<Value, HistoryFailure>> {
        self.0.page(request)
    }
}
impl Reader for SlackClient {
    fn fetch(&self, link: Link) -> BoxFuture<'_, Result<Vec<Entry>, LinkFailure>> {
        self.0.fetch(link)
    }
}
impl Downloader for SlackClient {
    fn download(&self, url: String, html: bool) -> BoxFuture<'_, Result<Download, FileFailure>> {
        self.0.download(url, html)
    }
    fn resolve(&self, file_id: String) -> BoxFuture<'_, Result<String, FileFailure>> {
        self.0.resolve(file_id)
    }
}

/// The owner, workspace and channels a client for `config` may use.
pub fn scope(config: &Config) -> Scope {
    Scope {
        owner: config.owner.slack_user.clone(),
        workspace: config.slack.workspace.clone(),
        channels: config.slack.channels.clone(),
    }
}
/// The owner's client for `config`, journaled to `store`.
pub fn client(
    config: &Config,
    store: Store,
    clock: Arc<dyn Clock>,
    token: String,
    timeout: Duration,
) -> Result<WebClient, Failure> {
    WebClient::new(
        scope(config),
        Arc::new(StoreJournal { store, clock }),
        token,
        timeout,
    )
}
pub fn matches_scope(web: &WebClient, config: &Config) -> bool {
    *web.scope() == scope(config)
}
/// Fridica's message metadata, so its own posts are recognized on intake.
pub fn metadata(meta: &Value) -> Value {
    json!({"event_type":"fridica_message","event_payload":{
        "v":meta["v"].as_u64().unwrap_or(2),"owner":meta["owner"].as_str().unwrap_or(""),
        "session":meta["session"].as_str().unwrap_or(""),"task_id":meta["session"].as_str().unwrap_or(""),
        "turn":meta["turn"].as_u64().unwrap_or(0),"status":meta["status"].as_str().unwrap_or(""),
        "kind":meta["kind"].as_str().unwrap_or("reply"),"worker":meta["worker"].as_str().unwrap_or("")}})
}
/// Posts only into the thread its session names, in a scoped channel.
async fn deliver(web: &WebClient, claim: ClaimedPost) -> Result<String, Failure> {
    let post = claim.post;
    let scope = web.scope();
    if !web.is_validated() {
        return Err(Failure::NotValidated);
    }
    if !scope.channel(&post.channel) {
        return Err(Failure::Scope);
    }
    let session = post
        .session_id
        .parse::<ThreadId>()
        .map_err(|_| Failure::Scope)?;
    if session.workspace.0 != scope.workspace
        || session.channel.0 != post.channel
        || post
            .thread_ts
            .as_deref()
            .is_some_and(|s| !timestamp(s) || s != session.root_ts.0)
    {
        return Err(Failure::Scope);
    }
    let context = Some(json!({"outbox":claim.id,"attempt":claim.attempt}));
    if post.kind == "upload" {
        let data = post.blob.ok_or(Failure::Configuration)?;
        return web
            .upload(
                Upload {
                    channel: post.channel,
                    thread_ts: post.thread_ts,
                    filename: post.filename,
                    data,
                },
                context,
            )
            .await;
    }
    if !matches!(
        post.kind.as_str(),
        "reply" | "notice" | "report" | "debrief_root" | "approval_notice" | "overseer"
    ) {
        return Err(Failure::Configuration);
    }
    web.post(
        Post {
            channel: post.channel,
            thread_ts: post.thread_ts,
            text: post.text,
            metadata: post.meta.as_ref().map(metadata),
        },
        context,
    )
    .await
}
/// Ambiguous outcomes are never retried: the post may already be in Slack.
impl Delivery for SlackClient {
    fn send(&self, post: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            match deliver(self, post).await {
                Ok(reference) => DeliveryOutcome::Sent { reference },
                Err(Failure::RateLimited { retry_after }) => {
                    DeliveryOutcome::RateLimited { retry_after }
                }
                Err(Failure::Rejected { code }) => DeliveryOutcome::Rejected { code },
                Err(Failure::Configuration | Failure::Scope | Failure::NotValidated) => {
                    DeliveryOutcome::Rejected {
                        code: "slack_not_authorized".into(),
                    }
                }
                Err(Failure::Ambiguous { code }) => DeliveryOutcome::Ambiguous { code },
                Err(failure) => DeliveryOutcome::Ambiguous {
                    code: match failure {
                        Failure::Timeout => "slack_timeout",
                        Failure::ResponseLimit => "slack_response_limit",
                        Failure::Recording => "slack_recording_failed",
                        Failure::Connection => "slack_connection_failed",
                        _ => "slack_invalid_response",
                    }
                    .into(),
                },
            }
        })
    }
}
#[cfg(test)]
#[path = "../../tests/support/slack_web.rs"]
mod tests;
