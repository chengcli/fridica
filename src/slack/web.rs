//! Owner-authenticated Slack Web API. Fixed endpoints, no transport retries or
//! redirects, and durable boundary records before I/O. Constructors do no I/O.
mod download;
use super::{
    catchup::{History, HistoryFailure, Method, PageRequest},
    ingress::{file_url, timestamp, ENVELOPE_LIMIT},
};
use crate::{
    config::Config,
    core::{
        delivery::{AdapterFuture, ClaimedPost, Delivery, DeliveryOutcome},
        time::Clock,
    },
    store::Store,
};
use reqwest::{
    header::{HeaderValue, AUTHORIZATION},
    Client, Url,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Failure {
    Configuration,
    NotValidated,
    Scope,
    Identity,
    Membership,
    Recording,
    Connection,
    Timeout,
    ResponseLimit,
    InvalidResponse,
    RateLimited { retry_after: f64 },
    Rejected { code: String },
    Ambiguous { code: String },
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Slack request failed: {self:?}")
    }
}
impl std::error::Error for Failure {}
type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Identity {
    pub owner: String,
    pub workspace: String,
    pub scopes: Option<BTreeSet<String>>,
}

#[derive(Clone)]
pub struct WebClient {
    client: Client,
    // Never Debug/Serialize the client or the credential. HeaderValue is sensitive.
    authorization: HeaderValue,
    config: Arc<Config>,
    store: Store,
    clock: Arc<dyn Clock>,
    validated: Arc<AtomicBool>,
    file_scopes: Arc<std::sync::RwLock<Option<BTreeSet<String>>>>,
    downloads: Arc<tokio::sync::Mutex<download::Cache>>,
    base: Url,
    #[cfg(test)]
    upload_origin: Option<Url>,
    #[cfg(test)]
    file_origin: Option<Url>,
}
#[derive(Clone, Copy)]
enum Api {
    Auth,
    SocketUrl,
    Channel,
    Post,
    UploadUrl,
    CompleteUpload,
    History,
    Replies,
}
impl Api {
    fn name(self) -> &'static str {
        match self {
            Self::Auth => "auth.test",
            Self::SocketUrl => "apps.connections.open",
            Self::Channel => "conversations.info",
            Self::Post => "chat.postMessage",
            Self::UploadUrl => "files.getUploadURLExternal",
            Self::CompleteUpload => "files.completeUploadExternal",
            Self::History => "conversations.history",
            Self::Replies => "conversations.replies",
        }
    }
}
struct Response {
    status: u16,
    retry_after: Option<String>,
    scopes: Option<String>,
    body: Vec<u8>,
}
impl WebClient {
    pub fn new(
        config: Arc<Config>,
        store: Store,
        clock: Arc<dyn Clock>,
        token: String,
        timeout: Duration,
    ) -> Result<Self> {
        if !token.starts_with("xoxp-")
            || token.len() < 6
            || token.bytes().any(|b| !b.is_ascii_graphic())
            || timeout.is_zero()
        {
            return Err(Failure::Configuration);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| Failure::Configuration)?;
        authorization.set_sensitive(true);
        let builder = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .referer(false)
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(10)));
        // Production preserves the owner's conventional proxy environment. Tests
        // isolate loopback traffic from the surrounding process environment.
        #[cfg(test)]
        let builder = builder.no_proxy();
        let client = builder.build().map_err(|_| Failure::Configuration)?;
        Ok(Self {
            client,
            authorization,
            config,
            store,
            clock,
            validated: Arc::new(AtomicBool::new(false)),
            file_scopes: Arc::new(std::sync::RwLock::new(None)),
            downloads: Arc::new(tokio::sync::Mutex::new(download::Cache::default())),
            base: Url::parse("https://slack.com/api/").map_err(|_| Failure::Configuration)?,
            #[cfg(test)]
            upload_origin: None,
            #[cfg(test)]
            file_origin: None,
        })
    }
    pub(crate) fn is_validated(&self) -> bool {
        self.validated.load(Ordering::Acquire)
    }
    pub(crate) fn matches_scope(&self, config: &Config) -> bool {
        self.config.owner.slack_user == config.owner.slack_user
            && self.config.slack.workspace == config.slack.workspace
            && self.config.slack.channels == config.slack.channels
    }
    /// The short-lived URL is a credential. Only Socket Mode receives it; neither
    /// HTTP records nor diagnostics retain the URL or app token.
    pub(crate) async fn socket_url(&self, app_token: &str, connection: &str) -> Result<String> {
        if !self.is_validated() {
            return Err(Failure::NotValidated);
        }
        if !app_token.starts_with("xapp-")
            || app_token.len() < 6
            || app_token.bytes().any(|b| !b.is_ascii_graphic())
        {
            return Err(Failure::Configuration);
        }
        let mut authorization = HeaderValue::from_str(&format!("Bearer {app_token}"))
            .map_err(|_| Failure::Configuration)?;
        authorization.set_sensitive(true);
        let app = Self {
            authorization,
            ..self.clone()
        };
        let response = decode(
            &app.api(
                Api::SocketUrl,
                json!({}),
                Some(json!({"connection":connection})),
            )
            .await?,
        )?;
        response["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or(Failure::InvalidResponse)
    }
    #[cfg(test)]
    pub(crate) fn test_endpoint(&mut self, base: Url) {
        self.base = base;
    }
    /// Read-only startup check; every configured channel must be accessible and
    /// report membership. Only a fully recorded successful check enables sends.
    pub async fn validate(&self) -> Result<Identity> {
        self.validated.store(false, Ordering::Release);
        let response = self.api(Api::Auth, json!({}), None).await?;
        let auth = decode(&response)?;
        if auth["user_id"] != self.config.owner.slack_user
            || auth["team_id"] != self.config.slack.workspace
            || auth.get("bot_id").is_some_and(|v| !v.is_null() && v != "")
        {
            return Err(Failure::Identity);
        }
        let token = self
            .authorization
            .to_str()
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .unwrap_or("");
        let scopes = safe_header(&response.scopes, token).map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect::<BTreeSet<_>>()
        });
        for channel in &self.config.slack.channels {
            let info = decode(
                &self
                    .api(Api::Channel, json!({"channel":channel}), None)
                    .await?,
            )?;
            if info["channel"]["id"] != *channel || info["channel"]["is_member"] != true {
                return Err(Failure::Membership);
            }
        }
        let identity = Identity {
            owner: self.config.owner.slack_user.clone(),
            workspace: self.config.slack.workspace.clone(),
            scopes,
        };
        let stored = identity
            .scopes
            .as_ref()
            .map(|s| s.iter().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or("unknown".into());
        self.store.call(move|c| {c.execute("INSERT INTO meta VALUES('slack_scopes',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[stored])?;Ok(())}).await.map_err(|_|Failure::Recording)?;
        *self.file_scopes.write().map_err(|_| Failure::Recording)? = identity.scopes.clone();
        self.validated.store(true, Ordering::Release);
        Ok(identity)
    }
    fn scope(&self, channel: &str) -> Result<()> {
        if !self.validated.load(Ordering::Acquire) {
            return Err(Failure::NotValidated);
        }
        if !self.config.slack.channels.iter().any(|c| c == channel) {
            return Err(Failure::Scope);
        }
        Ok(())
    }
    async fn deliver(&self, claim: ClaimedPost) -> Result<String> {
        let post = claim.post;
        self.scope(&post.channel)?;
        let session = post
            .session_id
            .parse::<crate::core::ids::ThreadId>()
            .map_err(|_| Failure::Scope)?;
        if session.workspace.0 != self.config.slack.workspace
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
            if post.filename.is_empty() {
                return Err(Failure::Configuration);
            }
            let ticket = decode(
                &self
                    .api(
                        Api::UploadUrl,
                        json!({"filename":post.filename,"length":data.len()}),
                        context.clone(),
                    )
                    .await?,
            )?;
            let id = ticket["file_id"]
                .as_str()
                .filter(|id| valid_file_id(id))
                .ok_or(Failure::InvalidResponse)?;
            let url = ticket["upload_url"]
                .as_str()
                .ok_or(Failure::InvalidResponse)?;
            let target = self.upload_url(url)?;
            let record = json!({"file_id":id,"length":data.len(),"sha256":format!("{:x}",<sha2::Sha256 as sha2::Digest>::digest(&data))});
            let request = self
                .client
                .post(target)
                .header("Content-Type", "application/octet-stream")
                .body(data);
            let uploaded = self
                .request("file_bytes", record, context.clone(), request)
                .await?;
            if uploaded.status != 200 {
                return Err(http_failure(&uploaded));
            }
            let mut body =
                json!({"files":[{"id":id,"title":post.filename}],"channel_id":post.channel});
            if let Some(ts) = post.thread_ts {
                body["thread_ts"] = json!(ts);
            }
            let confirmed = decode(&self.api(Api::CompleteUpload, body, context).await?)?;
            if !confirmed["files"]
                .as_array()
                .is_some_and(|files| files.len() == 1 && files[0]["id"] == id)
            {
                return Err(Failure::Ambiguous {
                    code: "unconfirmed_file".into(),
                });
            }
            return Ok(id.into());
        }
        if !matches!(
            post.kind.as_str(),
            "reply" | "notice" | "report" | "debrief_root" | "approval_notice" | "overseer"
        ) {
            return Err(Failure::Configuration);
        }
        let mut body = json!({"channel":post.channel,"text":post.text,"unfurl_links":false,"unfurl_media":false});
        if let Some(ts) = post.thread_ts {
            body["thread_ts"] = json!(ts);
        }
        if let Some(meta) = post.meta {
            body["metadata"] = metadata(&meta);
        }
        let response = decode(&self.api(Api::Post, body, context).await?)?;
        let ts = response["ts"]
            .as_str()
            .filter(|s| timestamp(s))
            .ok_or(Failure::Ambiguous {
                code: "unconfirmed_timestamp".into(),
            })?;
        if response
            .get("channel")
            .is_some_and(|channel| channel != &post.channel)
        {
            return Err(Failure::Ambiguous {
                code: "unexpected_channel".into(),
            });
        }
        Ok(ts.into())
    }
    fn upload_url(&self, value: &str) -> Result<Url> {
        let url = Url::parse(value).map_err(|_| Failure::InvalidResponse)?;
        #[cfg(test)]
        if let Some(origin) = &self.upload_origin {
            if url.origin() == origin.origin()
                && url.path().starts_with("/upload/")
                && url.username().is_empty()
                && url.password().is_none()
                && url.fragment().is_none()
            {
                return Ok(url);
            }
        }
        if !file_url(value) || !url.path().starts_with("/upload/") || url.fragment().is_some() {
            return Err(Failure::InvalidResponse);
        }
        Ok(url)
    }
    async fn api(&self, api: Api, body: Value, context: Option<Value>) -> Result<Response> {
        let url = self
            .base
            .join(api.name())
            .map_err(|_| Failure::Configuration)?;
        let request = match api {
            Api::Channel | Api::History | Api::Replies => self.client.get(url).query(&body),
            _ => self.client.post(url).json(&body),
        }
        .header(AUTHORIZATION, self.authorization.clone());
        self.request(api.name(), body, context, request).await
    }
    async fn request(
        &self,
        method: &'static str,
        body: Value,
        context: Option<Value>,
        request: reqwest::RequestBuilder,
    ) -> Result<Response> {
        let now = self.clock.now();
        if !now.is_finite() {
            return Err(Failure::Configuration);
        }
        let token = self
            .authorization
            .to_str()
            .unwrap_or("")
            .strip_prefix("Bearer ")
            .unwrap_or("");
        let mut record = json!({"method":method,"arguments":body,"context":context});
        redact(&mut record, token);
        let call=self.store.call(move|c| {c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_http_call',?,?,0)",params![now,record.to_string()])?;Ok(c.last_insert_rowid())}).await.map_err(|_|Failure::Recording)?;
        let result = read(request).await;
        let mut complete = !matches!(result, Err(Failure::ResponseLimit));
        let record = match &result {
            Ok(response) => {
                let token = self
                    .authorization
                    .to_str()
                    .unwrap_or("")
                    .strip_prefix("Bearer ")
                    .unwrap_or("");
                let body = match serde_json::from_slice::<Value>(&response.body) {
                    Ok(mut value) => {
                        redact(&mut value, token);
                        if method == "apps.connections.open" {
                            value
                                .as_object_mut()
                                .map(|v| v.insert("url".into(), json!("[socket credential]")));
                        }
                        json!({"json":value})
                    }
                    Err(_) if method == "apps.connections.open" => {
                        complete = false;
                        json!({"omitted":"invalid connection response may contain credentials"})
                    }
                    Err(_) => {
                        let mut bytes = response.body.clone();
                        // Preserve malformed/binary responses but scrub the actual
                        // credential if an error server reflects it.
                        scrub_bytes(&mut bytes, token.as_bytes());
                        json!({"bytes":bytes})
                    }
                };
                json!({"call":call,"status":response.status,"retry_after":safe_header(&response.retry_after,token),"scopes":safe_header(&response.scopes,token),"body":body})
            }
            Err(failure) => json!({"call":call,"failure":failure}),
        };
        // Timeouts/body failures may have discarded a prefix; those are not a
        // complete response corpus even though the transport outcome is known.
        if matches!(result, Err(Failure::Timeout | Failure::Connection)) {
            complete = false;
        }
        let now = self.clock.now();
        self.store.call(move|c| {
            let tx=c.transaction()?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_http_result',?,?,?)",params![now,record.to_string(),complete])?;
            tx.execute("UPDATE replay_events SET complete=? WHERE seq=?",params![complete,call])?;
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Recording)?;
        result
    }
}
fn safe_header(value: &Option<String>, token: &str) -> Option<String> {
    value
        .as_ref()
        .filter(|v| v.len() <= 4096 && v.bytes().all(|b| b.is_ascii_graphic() || b == b' '))
        .map(|v| v.replace(token, "[credential]"))
}
fn scrub_bytes(value: &mut Vec<u8>, token: &[u8]) {
    if token.is_empty() {
        return;
    }
    let mut clean = Vec::with_capacity(value.len());
    let mut offset = 0;
    while offset < value.len() {
        if value[offset..].starts_with(token) {
            clean.extend_from_slice(b"[credential]");
            offset += token.len();
        } else {
            clean.push(value[offset]);
            offset += 1;
        }
    }
    *value = clean;
}
fn redact(value: &mut Value, token: &str) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "token" | "access_token" | "refresh_token" | "upload_url"
                ) {
                    *value = json!("[credential]");
                } else {
                    redact(value, token);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact(item, token);
            }
        }
        Value::String(text) if !token.is_empty() => *text = text.replace(token, "[credential]"),
        _ => (),
    }
}
async fn read(request: reqwest::RequestBuilder) -> Result<Response> {
    let failure = |e: reqwest::Error| {
        if e.is_timeout() {
            Failure::Timeout
        } else {
            Failure::Connection
        }
    };
    let mut response = request.send().await.map_err(failure)?;
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let retry_after = header("retry-after");
    let scopes = header("x-oauth-scopes");
    if response
        .content_length()
        .is_some_and(|n| n > ENVELOPE_LIMIT as u64)
    {
        return Err(Failure::ResponseLimit);
    }
    let mut body = vec![];
    while let Some(chunk) = response.chunk().await.map_err(failure)? {
        if body.len() + chunk.len() > ENVELOPE_LIMIT {
            return Err(Failure::ResponseLimit);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Response {
        status,
        retry_after,
        scopes,
        body,
    })
}
fn valid_file_id(id: &str) -> bool {
    id.len() > 1
        && id.len() <= 64
        && id.starts_with('F')
        && id.bytes().all(|b| b.is_ascii_alphanumeric())
}
fn code(body: &Value) -> String {
    const KNOWN: &[&str] = &[
        "internal_error",
        "fatal_error",
        "request_timeout",
        "service_unavailable",
        "invalid_auth",
        "not_authed",
        "token_revoked",
        "token_expired",
        "missing_scope",
        "channel_not_found",
        "not_in_channel",
        "is_archived",
        "no_permission",
        "restricted_action",
        "invalid_arguments",
        "file_not_found",
        "file_uploads_disabled",
        "invalid_metadata_format",
        "msg_too_long",
        "ratelimited",
    ];
    body["error"]
        .as_str()
        .filter(|s| KNOWN.contains(s))
        .unwrap_or("slack_api_error")
        .into()
}
fn http_failure(response: &Response) -> Failure {
    if response.status == 429 {
        return Failure::RateLimited {
            retry_after: response
                .retry_after
                .as_ref()
                .and_then(|s| s.parse::<f64>().ok())
                .filter(|n| n.is_finite())
                .unwrap_or(30.),
        };
    }
    let body: Value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    let code = code(&body);
    if response.status >= 500
        || matches!(
            code.as_str(),
            "internal_error" | "fatal_error" | "request_timeout" | "service_unavailable"
        )
    {
        Failure::Ambiguous { code }
    } else if body["ok"] == false || (400..500).contains(&response.status) {
        Failure::Rejected { code }
    } else {
        Failure::InvalidResponse
    }
}
fn decode(response: &Response) -> Result<Value> {
    if response.status != 200 {
        return Err(http_failure(response));
    }
    let body: Value =
        serde_json::from_slice(&response.body).map_err(|_| Failure::InvalidResponse)?;
    if body["ok"] != true {
        return Err(http_failure(response));
    }
    Ok(body)
}
pub fn metadata(meta: &Value) -> Value {
    json!({"event_type":"fridica_message","event_payload":{
        "v":meta["v"].as_u64().unwrap_or(2),"owner":meta["owner"].as_str().unwrap_or(""),
        "session":meta["session"].as_str().unwrap_or(""),"task_id":meta["session"].as_str().unwrap_or(""),
        "turn":meta["turn"].as_u64().unwrap_or(0),"status":meta["status"].as_str().unwrap_or(""),
        "kind":meta["kind"].as_str().unwrap_or("reply"),"worker":meta["worker"].as_str().unwrap_or("")}})
}
impl Delivery for WebClient {
    fn send(&self, post: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            match self.deliver(post).await {
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
impl History for WebClient {
    fn page(
        &self,
        request: PageRequest,
    ) -> AdapterFuture<'_, std::result::Result<Value, HistoryFailure>> {
        Box::pin(async move {
            let operation = async {
                self.scope(&request.channel)?;
                if request.limit == 0
                    || request.limit > 200
                    || !timestamp(&request.oldest)
                    || request.ts.as_deref().is_some_and(|s| !timestamp(s))
                    || matches!(request.method, Method::Replies) != request.ts.is_some()
                {
                    return Err(Failure::Configuration);
                }
                let api = match request.method {
                    Method::History => Api::History,
                    Method::Replies => Api::Replies,
                };
                let mut body = serde_json::to_value(request).map_err(|_| Failure::Configuration)?;
                body.as_object_mut()
                    .ok_or(Failure::Configuration)?
                    .remove("method");
                decode(&self.api(api, body, None).await?)
            }
            .await;
            operation.map_err(|error| match error {
                Failure::Timeout => HistoryFailure::Timeout,
                Failure::Connection => HistoryFailure::Connection,
                Failure::RateLimited { retry_after } => HistoryFailure::RateLimited { retry_after },
                Failure::InvalidResponse | Failure::ResponseLimit => {
                    HistoryFailure::InvalidResponse
                }
                _ => HistoryFailure::Rejected,
            })
        })
    }
}
#[cfg(test)]
#[path = "../../tests/support/slack_web.rs"]
mod tests;
