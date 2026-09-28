//! Service scheduling over durable queues. Socket intake, history reads, runtime
//! passes and heartbeat writes progress independently; notifications only hasten
//! catch-up, and periodic scans recover coalesced notifications.
use super::runtime::Runtime;
use crate::{
    core::{
        delivery::{AdapterFuture, Delivery},
        parent::Parent,
    },
    slack::{
        catchup::{Catchup, History, HistoryFailure, RECENT, WINDOW},
        receiver::Receiver,
        socket::{self, SocketMode, Status},
    },
};
use rusqlite::params;
use serde::Serialize;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use tokio::{sync::watch, time::Instant};

/// Authenticated connection boundary. Implementations must only publish Connected
/// after validating the identity used by the shared history/delivery adapters.
pub trait Connection: Send + Sync {
    fn subscribe(&self) -> watch::Receiver<Status>;
    fn run(&self, stop: watch::Receiver<bool>) -> AdapterFuture<'_, Result<(), socket::Failure>>;
}
impl Connection for SocketMode {
    fn subscribe(&self) -> watch::Receiver<Status> {
        self.subscribe()
    }
    fn run(&self, stop: watch::Receiver<bool>) -> AdapterFuture<'_, Result<(), socket::Failure>> {
        Box::pin(self.run(stop))
    }
}
#[derive(Clone, Debug)]
pub struct Options {
    pub pass_interval: Duration,
    pub catchup_interval: Duration,
    pub history_timeout: Duration,
    pub heartbeat_interval: Duration,
    pub socket_stop_timeout: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            pass_interval: Duration::from_millis(250),
            catchup_interval: Duration::from_secs(300),
            history_timeout: Duration::from_secs(30),
            heartbeat_interval: Duration::from_secs(5),
            socket_stop_timeout: Duration::from_secs(5),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Failure {
    Configuration,
    Storage,
    Runtime,
    Socket { failure: socket::Failure },
    SocketStopped,
    Shutdown,
    Task,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "service failed: {self:?}")
    }
}
impl std::error::Error for Failure {}

pub struct Service<P: Parent, D: Delivery, H: History, C: Connection = SocketMode> {
    runtime: Arc<Runtime<P, D>>,
    connection: C,
    catchup: Catchup<H>,
    receiver: Receiver,
    options: Options,
}
impl<P: Parent + 'static, D: Delivery + 'static, H: History + 'static, C: Connection + 'static>
    Service<P, D, H, C>
{
    /// Constructors perform no external I/O. Bind all adapters to one exclusive
    /// Store and authenticated Slack identity. Prefer Runtime::slack_service for
    /// the production adapters, which share one WebClient automatically.
    pub fn new(
        runtime: Runtime<P, D>,
        history: Arc<H>,
        connection: impl FnOnce(Receiver) -> Result<C, socket::Failure>,
        options: Options,
    ) -> Result<Self, Failure> {
        if [
            options.pass_interval,
            options.catchup_interval,
            options.history_timeout,
            options.heartbeat_interval,
            options.socket_stop_timeout,
        ]
        .iter()
        .any(|d| d.is_zero() || *d > Duration::from_secs(86400))
        {
            return Err(Failure::Configuration);
        }
        let receiver = runtime.slack_receiver();
        let connection =
            connection(receiver.clone()).map_err(|failure| Failure::Socket { failure })?;
        let catchup = Catchup::new(receiver.clone(), history, options.history_timeout)
            .map_err(|_| Failure::Configuration)?;
        Ok(Self {
            runtime: Arc::new(runtime),
            connection,
            catchup,
            receiver,
            options,
        })
    }
    /// Authenticated control servers can retain this handle. The service owns
    /// pass/close scheduling; callers must not invoke those methods concurrently.
    pub fn runtime(&self) -> Arc<Runtime<P, D>> {
        self.runtime.clone()
    }

    /// Consumes the service: a stopped supervisor cannot be restarted. Dropping
    /// this future asks the owned coordinator to stop and finish worker cleanup;
    /// it does not detach a live socket or leave a parent loop running. Cleanup
    /// needs the Tokio runtime to remain alive (process crashes use DB recovery).
    pub async fn run(self, stop: watch::Receiver<bool>) -> Result<(), Failure> {
        let (cancel, cancelled) = watch::channel(false);
        let task = tokio::spawn(self.coordinate(stop, cancelled));
        // Keep the sender alive through the join. Dropping it also signals stop.
        let result = task.await.map_err(|_| Failure::Task)?;
        drop(cancel);
        result
    }
    async fn coordinate(
        self,
        mut stop: watch::Receiver<bool>,
        mut cancelled: watch::Receiver<bool>,
    ) -> Result<(), Failure> {
        let started = self.receiver.clock.now();
        let mut result = self.begin(started).await;
        if result.is_ok() {
            let (socket_stop, stopping) = watch::channel(false);
            let mut socket = Box::pin(self.connection.run(stopping));
            let mut socket_done = false;
            // These borrowed futures are dropped before close: actor JoinSets
            // cancel their parent calls, and uncertain deliveries remain claimed.
            result = {
                let work = self.work_loop();
                let history = self.history_loop(started);
                let heartbeat = self.heartbeat_loop();
                tokio::pin!(work, history, heartbeat);
                tokio::select! { biased;
                    _=socket::stopped(&mut stop)=>Ok(()),
                    _=socket::stopped(&mut cancelled)=>Ok(()),
                    outcome=&mut socket=>{
                        socket_done=true;
                        Err(outcome.err().map(|failure|Failure::Socket{failure}).unwrap_or(Failure::SocketStopped))
                    },
                    outcome=&mut work=>outcome,
                    outcome=&mut history=>outcome,
                    outcome=&mut heartbeat=>outcome,
                }
            };
            socket_stop.send_replace(true);
            if !socket_done {
                let closed =
                    tokio::time::timeout(self.options.socket_stop_timeout, &mut socket).await;
                if !matches!(closed, Ok(Ok(()))) {
                    let failure = match closed {
                        Ok(Err(failure)) => Failure::Socket { failure },
                        _ => Failure::Shutdown,
                    };
                    // Preserve the primary failure but also record cleanup faults.
                    let recorded = self
                        .health("service_socket_shutdown_failed", &failure)
                        .await;
                    if result.is_ok() {
                        result = Err(failure);
                    }
                    if recorded.is_err() {
                        result = Err(Failure::Storage);
                    }
                }
            }
        }
        // Always attempt every worker close, even if socket/startup/storage failed.
        // Supervisor applies bounded per-worker termination and records failures.
        if self.runtime.close().await.is_err() {
            let recorded = self
                .health("service_worker_shutdown_failed", &Failure::Shutdown)
                .await;
            if result.is_ok() {
                result = Err(Failure::Shutdown);
            }
            if recorded.is_err() {
                result = Err(Failure::Storage);
            }
        }
        self.end(result.as_ref().err()).await?;
        result
    }
    async fn work_loop(&self) -> Result<(), Failure> {
        connected(&mut self.connection.subscribe()).await?;
        loop {
            self.runtime.pass().await.map_err(|_| Failure::Runtime)?;
            // Delay after completion, not a burst of ticks after a slow parent.
            tokio::time::sleep(self.options.pass_interval).await;
        }
    }
    async fn history_loop(&self, started: f64) -> Result<(), Failure> {
        let mut status = self.connection.subscribe();
        connected(&mut status).await?;
        let mut window = WINDOW;
        loop {
            let outcome = self.catchup.run(window, Some(started)).await;
            let mut retry_floor = Duration::ZERO;
            let detail = match &outcome {
                Ok(progress) => {
                    json!({"window":window,"added":progress.added,"incomplete_channels":progress.incomplete_channels})
                }
                Err(error) => {
                    if let Some(HistoryFailure::RateLimited { retry_after }) =
                        error.downcast_ref::<HistoryFailure>()
                    {
                        if retry_after.is_finite() && *retry_after > 0. {
                            retry_floor = Duration::try_from_secs_f64(retry_after.max(1.))
                                .map_err(|_| Failure::Configuration)?;
                        }
                    }
                    json!({"window":window,"failure":"catchup_failed","retry_after":retry_floor.as_secs_f64()})
                }
            };
            let failed = outcome
                .as_ref()
                .map_or(true, |p| !p.incomplete_channels.is_empty());
            self.history_result(detail, failed).await?;
            window = if failed { WINDOW } else { RECENT };
            let now = Instant::now();
            let next = now
                .checked_add(self.options.catchup_interval.max(retry_floor))
                .ok_or(Failure::Configuration)?;
            let earliest = now.checked_add(retry_floor).ok_or(Failure::Configuration)?;
            loop {
                tokio::select! {
                    _=tokio::time::sleep_until(next)=>break,
                    changed=status.changed()=>{
                        changed.map_err(|_|Failure::SocketStopped)?;
                        let reconnected=*status.borrow_and_update()==Status::Connected;
                        if reconnected {
                            // Reconnect must not bypass an HTTP Retry-After.
                            tokio::time::sleep_until(earliest).await;
                            break;
                        }
                    }
                }
            }
        }
    }
    async fn heartbeat_loop(&self) -> Result<(), Failure> {
        loop {
            tokio::time::sleep(self.options.heartbeat_interval).await;
            let now = self.receiver.clock.now();
            if !now.is_finite() {
                return Err(Failure::Configuration);
            }
            self.receiver
                .store
                .call(move |c| {
                    c.execute("UPDATE runtime SET heartbeat_at=? WHERE id=1", [now])?;
                    Ok(())
                })
                .await
                .map_err(|_| Failure::Storage)?;
        }
    }
    async fn begin(&self, started: f64) -> Result<(), Failure> {
        if !started.is_finite() {
            return Err(Failure::Configuration);
        }
        let fingerprint = self.receiver.config.fingerprint.clone();
        let observe = self.runtime.observe_only();
        self.receiver.store.call(move|c| {
            let tx=c.transaction()?;
            let previous:Option<String>=tx.query_row("SELECT (SELECT slack_status FROM runtime WHERE id=1)",[],|r|r.get(0))?;
            if previous.as_deref().is_some_and(|s|s!="stopped") {
                tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('service_interrupted',?,?)",params![json!({"previous":previous}).to_string(),started])?;
            }
            // No control endpoint is advertised before a control server exists.
            tx.execute("INSERT INTO runtime(id,pid,started_at,heartbeat_at,slack_status,observe_only,control_socket,config_fingerprint) VALUES(1,?,?,?,'starting',?,'',?) ON CONFLICT(id) DO UPDATE SET pid=excluded.pid,started_at=excluded.started_at,heartbeat_at=excluded.heartbeat_at,slack_status=excluded.slack_status,observe_only=excluded.observe_only,control_socket='',config_fingerprint=excluded.config_fingerprint",params![std::process::id(),started,started,observe,fingerprint])?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('service_start',?,?)",params![started,json!({"observe_only":observe}).to_string()])?;
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Storage)
    }
    async fn end(&self, failure: Option<&Failure>) -> Result<(), Failure> {
        let now = self.receiver.clock.now();
        let detail = json!({"failure":failure});
        let failed = failure.is_some();
        self.receiver.store.call(move|c| {
            let tx=c.transaction()?;
            tx.execute("UPDATE runtime SET slack_status='stopped',heartbeat_at=? WHERE id=1",[now])?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('service_stop',?,?)",params![now,detail.to_string()])?;
            if failed { tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('service_failed',?,?)",params![detail.to_string(),now])?; }
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Storage)
    }
    async fn history_result(&self, detail: serde_json::Value, failed: bool) -> Result<(), Failure> {
        let now = self.receiver.clock.now();
        self.receiver.store.call(move|c| {
            let tx=c.transaction()?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('service_catchup',?,?)",params![now,detail.to_string()])?;
            if failed { tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('service_catchup_failed',?,?)",params![detail.to_string(),now])?; }
            tx.commit()?;Ok(())
        }).await.map_err(|_|Failure::Storage)
    }
    async fn health(&self, kind: &'static str, failure: &Failure) -> Result<(), Failure> {
        let detail = serde_json::to_string(failure).map_err(|_| Failure::Storage)?;
        let now = self.receiver.clock.now();
        self.receiver
            .store
            .call(move |c| {
                c.execute(
                    "INSERT INTO health_events(kind,details_json,created) VALUES(?,?,?)",
                    params![kind, detail, now],
                )?;
                Ok(())
            })
            .await
            .map_err(|_| Failure::Storage)
    }
}
async fn connected(status: &mut watch::Receiver<Status>) -> Result<(), Failure> {
    loop {
        if *status.borrow_and_update() == Status::Connected {
            return Ok(());
        }
        status.changed().await.map_err(|_| Failure::SocketStopped)?;
    }
}
