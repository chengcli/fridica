//! Socket Mode owns one bounded WebSocket at a time. All reconnects get a fresh
//! ticket; neither tickets nor credential-bearing transport errors reach storage.
use super::{
    ingress::ENVELOPE_LIMIT,
    receiver::Receiver,
    web::{Failure as WebFailure, WebClient},
};
use futures_util::{SinkExt, StreamExt};
use reqwest::Url;
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{
    net::TcpStream,
    sync::{watch, Mutex},
    time::Instant,
};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{protocol::WebSocketConfig, Error as WsError, Message},
    MaybeTlsStream, WebSocketStream,
};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Result<T> = std::result::Result<T, Failure>;
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Failure {
    Configuration,
    AlreadyRunning,
    Storage,
    Authentication,
    LinkDisabled,
    Connection,
    Protocol,
    FrameLimit,
    Timeout,
    Intake,
    Send,
    RateLimited { retry_after: f64 },
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Socket Mode failed: {self:?}")
    }
}
impl std::error::Error for Failure {}
#[derive(Clone, Debug)]
pub struct Options {
    pub connect_timeout: Duration,
    pub hello_timeout: Duration,
    pub io_timeout: Duration,
    pub ping_interval: Duration,
    pub pong_timeout: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub stable_after: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(20),
            hello_timeout: Duration::from_secs(10),
            io_timeout: Duration::from_secs(5),
            ping_interval: Duration::from_secs(20),
            pong_timeout: Duration::from_secs(10),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            stable_after: Duration::from_secs(60),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Connecting,
    Connected,
    Reconnecting,
    Stopped,
}
pub struct SocketMode {
    web: Arc<WebClient>,
    receiver: Receiver,
    app_token: String,
    options: Options,
    running: Mutex<()>,
    status: watch::Sender<Status>,
    #[cfg(test)]
    test_origin: Option<Url>,
}
struct StatusGuard<'a>(&'a watch::Sender<Status>);
impl Drop for StatusGuard<'_> {
    fn drop(&mut self) {
        self.0.send_replace(Status::Stopped);
    }
}
enum End {
    Refresh,
}
impl SocketMode {
    pub fn new(
        web: Arc<WebClient>,
        receiver: Receiver,
        app_token: String,
        options: Options,
    ) -> Result<Self> {
        if !web.matches_scope(&receiver.config)
            || !app_token.starts_with("xapp-")
            || app_token.len() < 6
            || app_token.bytes().any(|b| !b.is_ascii_graphic())
            || [
                options.connect_timeout,
                options.hello_timeout,
                options.io_timeout,
                options.ping_interval,
                options.pong_timeout,
                options.backoff_min,
                options.backoff_max,
                options.stable_after,
            ]
            .iter()
            .any(Duration::is_zero)
            || options.backoff_min > options.backoff_max
        {
            return Err(Failure::Configuration);
        }
        let (status, _) = watch::channel(Status::Stopped);
        Ok(Self {
            web,
            receiver,
            app_token,
            options,
            running: Mutex::new(()),
            status,
            #[cfg(test)]
            test_origin: None,
        })
    }
    pub fn subscribe(&self) -> watch::Receiver<Status> {
        self.status.subscribe()
    }
    /// Dropping the stop sender also stops the service. A dropped run future
    /// closes its socket; durable intents allow startup to diagnose interruptions.
    pub async fn run(&self, mut stop: watch::Receiver<bool>) -> Result<()> {
        let _running = self
            .running
            .try_lock()
            .map_err(|_| Failure::AlreadyRunning)?;
        let _status = StatusGuard(&self.status);
        let result = tokio::select! {biased;
            _=stopped(&mut stop)=>Ok(()),
            result=self.reconnect()=>result,
        };
        self.state(Status::Stopped, result.as_ref().err()).await?;
        result
    }
    async fn reconnect(&self) -> Result<()> {
        self.recover().await?;
        let mut failures = 0u32;
        loop {
            self.state(Status::Connecting, None).await?;
            let mut connected_at = None;
            let result = async {
                if !self.web.is_validated() {
                    self.web.validate().await.map_err(web_failure)?;
                }
                self.connection(&mut connected_at).await
            }
            .await;
            if matches!(
                result,
                Err(Failure::Storage
                    | Failure::Authentication
                    | Failure::Configuration
                    | Failure::LinkDisabled)
            ) {
                return result.map(|_| ());
            }
            if connected_at.is_some_and(|at: Instant| at.elapsed() >= self.options.stable_after) {
                failures = 0;
            }
            let delay = match &result {
                Err(Failure::RateLimited { retry_after }) => {
                    Duration::from_secs_f64(retry_after.clamp(1., 3600.))
                }
                _ => self
                    .options
                    .backoff_min
                    .saturating_mul(1u32 << failures.min(20))
                    .min(self.options.backoff_max),
            };
            failures = failures.saturating_add(1);
            self.state(Status::Reconnecting, result.as_ref().err())
                .await?;
            self.event(
                "slack_socket_backoff",
                json!({"seconds":delay.as_secs_f64()}),
                true,
            )
            .await?;
            tokio::time::sleep(delay).await;
        }
    }
    async fn connection(&self, connected_at: &mut Option<Instant>) -> Result<End> {
        let connection = self.receiver.ids.next("socket");
        let ticket = self
            .web
            .socket_url(&self.app_token, &connection)
            .await
            .map_err(web_failure)?;
        let url = self.url(&ticket)?;
        let call = self
            .event(
                "slack_socket_connect",
                json!({"connection":connection}),
                false,
            )
            .await?;
        let config = WebSocketConfig::default()
            .max_message_size(Some(ENVELOPE_LIMIT))
            .max_frame_size(Some(ENVELOPE_LIMIT))
            .write_buffer_size(0)
            .max_write_buffer_size(65536);
        let result = tokio::time::timeout(
            self.options.connect_timeout,
            connect_async_with_config(url.as_str(), Some(config), false),
        )
        .await;
        let result = match result {
            Ok(Ok((socket, _))) => Ok(socket),
            Ok(Err(_)) => Err(Failure::Connection),
            Err(_) => Err(Failure::Timeout),
        };
        self.finish(call,"slack_socket_connect_result",json!({"connection":connection,"connected":result.is_ok(),"failure":result.as_ref().err()})).await?;
        let mut socket = result?;
        let mut hello = false;
        let hello_deadline = Instant::now() + self.options.hello_timeout;
        let mut next_ping = Instant::now() + self.options.ping_interval;
        let mut pending_pong: Option<(Instant, Vec<u8>)> = None;
        loop {
            let timer = if !hello {
                hello_deadline
            } else {
                pending_pong.as_ref().map(|p| p.0).unwrap_or(next_ping)
            };
            let frame = tokio::select! {
                frame=socket.next()=>frame,
                _=tokio::time::sleep_until(timer)=>{
                    if !hello || pending_pong.is_some() {return Err(Failure::Timeout);}
                    let payload=self.receiver.ids.next("ping").into_bytes();
                    self.send(&mut socket,Message::Ping(payload.clone().into())).await?;
                    pending_pong=Some((Instant::now()+self.options.pong_timeout,payload));
                    continue;
                }
            };
            match frame {
                None | Some(Ok(Message::Close(_))) => return Err(Failure::Connection),
                Some(Err(WsError::Capacity(_))) => return Err(Failure::FrameLimit),
                Some(Err(_)) => return Err(Failure::Connection),
                Some(Ok(Message::Ping(_))) => {
                    // Tungstenite queues the matching Pong while reading a Ping.
                    tokio::time::timeout(self.options.io_timeout, socket.flush())
                        .await
                        .map_err(|_| Failure::Timeout)?
                        .map_err(|_| Failure::Send)?;
                }
                Some(Ok(Message::Pong(bytes))) => {
                    if pending_pong
                        .as_ref()
                        .is_some_and(|(_, expected)| expected.as_slice() == bytes.as_ref())
                    {
                        pending_pong = None;
                        next_ping = Instant::now() + self.options.ping_interval;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    let value: Value =
                        serde_json::from_str(&text).map_err(|_| Failure::Protocol)?;
                    match value["type"].as_str() {
                        Some("hello") if !hello => {
                            hello = true;
                            self.event(
                                "slack_socket_hello",
                                json!({"connection":connection}),
                                true,
                            )
                            .await?;
                            self.state(Status::Connected, None).await?;
                            *connected_at = Some(Instant::now());
                        }
                        Some("disconnect") => {
                            let reason = match value["reason"].as_str() {
                                Some("warning") => "warning",
                                Some("refresh_requested") => "refresh_requested",
                                Some("link_disabled") => "link_disabled",
                                _ => "unknown",
                            };
                            self.event(
                                "slack_socket_disconnect",
                                json!({"connection":connection,"reason":reason}),
                                true,
                            )
                            .await?;
                            if reason == "link_disabled" {
                                return Err(Failure::LinkDisabled);
                            }
                            if reason != "warning" {
                                return Ok(End::Refresh);
                            }
                        }
                        Some("hello") => return Err(Failure::Protocol),
                        _ if !hello => return Err(Failure::Protocol),
                        _ => {
                            let ack = tokio::time::timeout(
                                self.options.io_timeout,
                                self.receiver.receive(text.as_bytes()),
                            )
                            .await
                            .map_err(|_| Failure::Timeout)?
                            .map_err(|_| Failure::Intake)?;
                            if let Some(ack) = ack {
                                let call=self.event("slack_ack_call",json!({"connection":connection,"envelope_id":ack.envelope_id}),false).await?;
                                let result = self
                                    .send(
                                        &mut socket,
                                        Message::Text(
                                            serde_json::to_string(&ack)
                                                .map_err(|_| Failure::Protocol)?
                                                .into(),
                                        ),
                                    )
                                    .await;
                                self.finish(call,"slack_ack_result",json!({"connection":connection,"envelope_id":ack.envelope_id,"outcome":if result.is_ok() {"written"} else {"uncertain"}})).await?;
                                result?;
                            }
                        }
                    }
                }
                Some(Ok(_)) => return Err(Failure::Protocol),
            }
        }
    }
    async fn send(&self, socket: &mut Socket, message: Message) -> Result<()> {
        tokio::time::timeout(self.options.io_timeout, socket.send(message))
            .await
            .map_err(|_| Failure::Timeout)?
            .map_err(|_| Failure::Send)
    }
    fn url(&self, value: &str) -> Result<Url> {
        if value.len() > 8192
            || value
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        {
            return Err(Failure::Configuration);
        }
        let url = Url::parse(value).map_err(|_| Failure::Configuration)?;
        if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
            return Err(Failure::Configuration);
        }
        #[cfg(test)]
        if self
            .test_origin
            .as_ref()
            .is_some_and(|origin| origin.origin() == url.origin())
        {
            return Ok(url);
        }
        if url.scheme() != "wss"
            || url.port().is_some_and(|p| p != 443)
            || url.path() != "/link/"
            || !url
                .query_pairs()
                .any(|(k, v)| k == "ticket" && !v.is_empty())
        {
            return Err(Failure::Configuration);
        }
        let host = url.host_str().ok_or(Failure::Configuration)?;
        if !host.ends_with(".slack.com")
            || !host.split('.').all(|label| {
                !label.is_empty()
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return Err(Failure::Configuration);
        }
        Ok(url)
    }
    async fn event(&self, kind: &'static str, value: Value, complete: bool) -> Result<i64> {
        let now = self.receiver.clock.now();
        if !now.is_finite() {
            return Err(Failure::Configuration);
        }
        self.receiver
            .store
            .call(move |c| {
                c.execute(
                    "INSERT INTO replay_events(kind,time,payload_json,complete) VALUES(?,?,?,?)",
                    params![kind, now, value.to_string(), complete],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .map_err(|_| Failure::Storage)
    }
    async fn finish(&self, call: i64, kind: &'static str, value: Value) -> Result<()> {
        let now = self.receiver.clock.now();
        self.receiver
            .store
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                    "INSERT INTO replay_events(kind,time,payload_json) VALUES(?,?,?)",
                    params![kind, now, json!({"call":call,"result":value}).to_string()],
                )?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?", [call])?;
                tx.commit()?;
                Ok(())
            })
            .await
            .map_err(|_| Failure::Storage)
    }
    async fn recover(&self) -> Result<()> {
        let now = self.receiver.clock.now();
        self.receiver.store.call(move|c| {
            let tx=c.transaction()?;
            let previous:Option<String>=tx.query_row("SELECT (SELECT value FROM meta WHERE key='slack_status')",[],|r|r.get(0))?;
            if previous.as_deref().is_some_and(|s|matches!(s,"connecting"|"connected"|"reconnecting")) {
                tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('slack_socket_interrupted',?,?)",params![json!({"previous":previous}).to_string(),now])?;
            }
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Storage)
    }
    async fn state(&self, status: Status, failure: Option<&Failure>) -> Result<()> {
        let value = json!({"status":status,"failure":failure});
        let now = self.receiver.clock.now();
        let name = serde_json::to_value(status)
            .map_err(|_| Failure::Storage)?
            .as_str()
            .ok_or(Failure::Storage)?
            .to_string();
        let failed = failure.is_some();
        // Invalid/oversized frames and cancelled intake may have lost payload
        // bytes; the private tape must not advertise exact replay for that gap.
        let complete = !matches!(
            failure,
            Some(Failure::Protocol | Failure::FrameLimit | Failure::Intake | Failure::Timeout)
        );
        self.receiver.store.call(move|c| {
            let tx=c.transaction()?;
            tx.execute("INSERT INTO meta VALUES('slack_status',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[&name])?;
            tx.execute("UPDATE runtime SET slack_status=? WHERE id=1",[name])?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_socket_state',?,?,?)",params![now,value.to_string(),complete])?;
            if failed {tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('slack_socket_failure',?,?)",params![value.to_string(),now])?;}
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Storage)?;
        self.status.send_replace(status);
        Ok(())
    }
}
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}
fn web_failure(failure: WebFailure) -> Failure {
    match failure {
        WebFailure::RateLimited { retry_after } => Failure::RateLimited { retry_after },
        WebFailure::Recording => Failure::Storage,
        WebFailure::Configuration | WebFailure::Scope => Failure::Configuration,
        WebFailure::NotValidated
        | WebFailure::Identity
        | WebFailure::Membership
        | WebFailure::Rejected { .. } => Failure::Authentication,
        _ => Failure::Connection,
    }
}
#[cfg(test)]
#[path = "../../tests/support/slack_socket.rs"]
mod tests;
