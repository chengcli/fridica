use fridica::{
    config::{loader, Config, LoadContext},
    control::{api::Api, client::Client, server::Access},
    core::{
        delivery::*,
        parent::*,
        time::{ReplayClock, SequenceIds},
        worker::*,
        Authority,
    },
    daemon::ControlServer,
    slack::{
        catchup::{History, HistoryFailure, PageRequest},
        receiver::Receiver,
        socket::{self, Status},
    },
    store::Store,
    threads::{
        controls::Control,
        runtime::{Adapters, Runtime},
        service::{Connection, Failure, Lifecycle, Options, Service},
    },
    workers::protocol::*,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{watch, Notify, Semaphore};

type TestService = Service<ParentStub, Sink, Pages, Wire>;
struct Active<'a>(&'a AtomicBool);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn control_endpoint_is_live_before_slack_and_removed_after_observer_stop() {
    let mut h = Harness::new(true, options()).await;
    let service = h.service.take().unwrap();
    let config = service.runtime().config();
    let controls = ControlServer::new(
        config.clone(),
        Arc::new(Api::new(service.runtime())),
        Access::OwnerPeer,
        Default::default(),
    );
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(service.run_with_lifecycle(rx, controls));
    h.until("SELECT count(*) FROM runtime WHERE control_socket!=''", 1)
        .await;
    let client = Client::new(&config.state.control_socket, None).unwrap();
    let status = client.request("GET", "/status", None).await.unwrap();
    assert_eq!(status["observe_only"], true);
    let endpoint: String = h
        .store
        .call(|c| Ok(c.query_row("SELECT control_socket FROM runtime", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(endpoint, config.state.control_socket.to_string_lossy());
    assert!(h.pages.requests.lock().unwrap().is_empty());
    h.connect();
    h.intake("9999.1").await;
    h.until(
        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
        1,
    )
    .await;
    assert_eq!(h.scalar("SELECT count(*) FROM obligations").await, 1);
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.sink.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.workers.created.load(Ordering::SeqCst), 0);
    finish(stop, task).await.unwrap();
    assert!(!config.state.control_socket.exists());
    assert_eq!(
        h.scalar(
            "SELECT count(*) FROM runtime WHERE slack_status='stopped' AND control_socket='' "
        )
        .await,
        1
    );
}

#[tokio::test]
async fn control_bind_or_advertisement_failure_stops_before_slack_io() {
    for fail_advertise in [false, true] {
        let mut h = Harness::new(true, options()).await;
        let service = h.service.take().unwrap();
        let config = service.runtime().config();
        if fail_advertise {
            h.store.call(|c| { c.execute_batch("CREATE TRIGGER fail_endpoint BEFORE UPDATE ON runtime WHEN NEW.control_socket!='' BEGIN SELECT RAISE(ABORT,'private fault'); END;")?; Ok(()) }).await.unwrap();
        } else {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(config.state.control_socket.parent().unwrap())
                .unwrap();
            std::fs::write(&config.state.control_socket, "do not replace").unwrap();
        }
        let controls = ControlServer::new(
            config.clone(),
            Arc::new(Api::new(service.runtime())),
            Access::OwnerPeer,
            Default::default(),
        );
        let (_stop, rx) = watch::channel(false);
        let result = service.run_with_lifecycle(rx, controls).await;
        assert_eq!(
            result,
            Err(if fail_advertise {
                Failure::Storage
            } else {
                Failure::HostService
            })
        );
        assert_eq!(h.wire.0.starts.load(Ordering::SeqCst), 0);
        assert!(h.pages.requests.lock().unwrap().is_empty());
        assert_eq!(
            h.scalar(
                "SELECT count(*) FROM runtime WHERE slack_status='stopped' AND control_socket='' "
            )
            .await,
            1
        );
        if fail_advertise {
            assert!(!config.state.control_socket.exists());
        } else {
            assert_eq!(
                std::fs::read_to_string(&config.state.control_socket).unwrap(),
                "do not replace"
            );
        }
    }
}

struct DelayedControl {
    workers: Arc<Workers>,
    entered: Semaphore,
    release: Semaphore,
    finished: AtomicBool,
}
impl fridica::control::Backend for DelayedControl {
    fn request(
        &self,
        _: fridica::control::Request,
        _: Authority,
    ) -> AdapterFuture<'_, fridica::control::Response> {
        Box::pin(async move {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            assert!(
                self.workers.alive.load(Ordering::SeqCst),
                "worker cleanup raced an accepted control"
            );
            self.finished.store(true, Ordering::SeqCst);
            fridica::control::Response::ok(json!({}))
        })
    }
}
#[tokio::test]
async fn cancellation_drains_accepted_controls_before_worker_cleanup() {
    let mut h = Harness::new(false, options()).await;
    *h.parent.response.lock().unwrap() = json!({"delegations":[{"brief":"wait", "machine":"local","workspace":"project","backend":"codex"}]});
    let service = h.service.take().unwrap();
    let config = service.runtime().config();
    let backend = Arc::new(DelayedControl {
        workers: h.workers.clone(),
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        finished: AtomicBool::new(false),
    });
    let controls = ControlServer::new(
        config.clone(),
        backend.clone(),
        Access::OwnerPeer,
        Default::default(),
    );
    let (_stop, rx) = watch::channel(false);
    let task = tokio::spawn(service.run_with_lifecycle(rx, controls));
    h.until("SELECT count(*) FROM runtime WHERE control_socket!=''", 1)
        .await;
    h.connect();
    h.intake("9999.1").await;
    wait_for(|| h.workers.active.load(Ordering::SeqCst)).await;
    let client = Client::new(&config.state.control_socket, None).unwrap();
    let request = tokio::spawn(async move {
        client
            .request("POST", "/threads/any/pause", Some(json!({})))
            .await
    });
    tokio::time::timeout(Duration::from_secs(4), backend.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    wait_for(|| !h.wire.0.active.load(Ordering::SeqCst)).await;
    // The public run future is gone, but its coordinator still owns the server.
    assert_eq!(h.workers.closed.load(Ordering::SeqCst), 0);
    backend.release.add_permits(1);
    h.until(
        "SELECT count(*) FROM runtime WHERE slack_status='stopped' AND control_socket=''",
        1,
    )
    .await;
    assert!(backend.finished.load(Ordering::SeqCst));
    assert!(h.workers.closed.load(Ordering::SeqCst) > 0);
    assert!(!h.workers.alive.load(Ordering::SeqCst));
    assert!(!config.state.control_socket.exists());
    let _ = request.await.unwrap(); // Disconnect may hide a completed control.
}

struct FailingHost {
    stopped: Arc<AtomicBool>,
}

struct PendingHost {
    entered: Arc<Semaphore>,
    stopped: Arc<AtomicBool>,
}
impl Lifecycle for PendingHost {
    fn start(&mut self) -> AdapterFuture<'_, Result<Option<std::path::PathBuf>, Failure>> {
        Box::pin(async {
            self.entered.add_permits(1);
            std::future::pending().await
        })
    }
    fn stop(&mut self) -> AdapterFuture<'_, Result<(), Failure>> {
        Box::pin(async {
            self.stopped.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}
#[tokio::test]
async fn stop_before_or_during_host_start_never_opens_slack_and_cleans_partial_start() {
    for prestopped in [true, false] {
        let mut h = Harness::new(true, options()).await;
        let stopped = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(Semaphore::new(0));
        let (_stop, rx) = watch::channel(prestopped);
        let task = tokio::spawn(h.service.take().unwrap().run_with_lifecycle(
            rx,
            PendingHost {
                entered: entered.clone(),
                stopped: stopped.clone(),
            },
        ));
        if prestopped {
            tokio::time::timeout(Duration::from_secs(4), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(entered.available_permits(), 0);
        } else {
            tokio::time::timeout(Duration::from_secs(4), entered.acquire())
                .await
                .unwrap()
                .unwrap()
                .forget();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            h.until(
                "SELECT count(*) FROM runtime WHERE slack_status='stopped'",
                1,
            )
            .await;
        }
        assert!(stopped.load(Ordering::SeqCst));
        assert_eq!(h.wire.0.starts.load(Ordering::SeqCst), 0);
        assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    }
}
impl Lifecycle for FailingHost {
    fn start(&mut self) -> AdapterFuture<'_, Result<Option<std::path::PathBuf>, Failure>> {
        Box::pin(async { Ok(None) })
    }
    fn stop(&mut self) -> AdapterFuture<'_, Result<(), Failure>> {
        Box::pin(async {
            self.stopped.store(true, Ordering::SeqCst);
            Err(Failure::HostService)
        })
    }
    fn failed(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async {})
    }
}
#[tokio::test]
async fn failed_host_service_stops_scheduler_and_audits_shutdown_failure() {
    let mut h = Harness::new(true, options()).await;
    let stopped = Arc::new(AtomicBool::new(false));
    let (_stop, rx) = watch::channel(false);
    let result = h
        .service
        .take()
        .unwrap()
        .run_with_lifecycle(
            rx,
            FailingHost {
                stopped: stopped.clone(),
            },
        )
        .await;
    assert_eq!(result, Err(Failure::HostService));
    assert!(stopped.load(Ordering::SeqCst));
    assert_eq!(
        h.scalar("SELECT count(*) FROM health_events WHERE kind='service_host_shutdown_failed'")
            .await,
        1
    );
    assert_eq!(
        h.scalar("SELECT count(*) FROM runtime WHERE slack_status='stopped'")
            .await,
        1
    );
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
}
struct ParentStub {
    calls: AtomicUsize,
    active: AtomicBool,
    block: AtomicBool,
    release: Semaphore,
    response: Mutex<Value>,
}
impl Parent for ParentStub {
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.store(true, Ordering::SeqCst);
            let _active = Active(&self.active);
            if self.block.load(Ordering::SeqCst) {
                self.release.acquire().await.unwrap().forget();
            }
            let mut response = self.response.lock().unwrap().clone();
            if response["reply"].is_object() {
                response["reply"]["answers"] = json!(request
                    .obligations
                    .iter()
                    .map(|o| o["id"].clone())
                    .collect::<Vec<_>>());
            }
            Ok(response)
        })
    }
}
#[derive(Default)]
struct Sink {
    calls: AtomicUsize,
    block: AtomicBool,
    active: AtomicBool,
}
impl Delivery for Sink {
    fn send(&self, p: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.store(true, Ordering::SeqCst);
            let _active = Active(&self.active);
            if self.block.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            DeliveryOutcome::Sent {
                reference: format!("200.{}", p.id),
            }
        })
    }
}
#[derive(Default)]
struct Workers {
    created: AtomicUsize,
    closed: AtomicUsize,
    active: AtomicBool,
    alive: AtomicBool,
}
struct WorkerStub(Arc<Workers>);
impl Worker for WorkerStub {
    fn alive(&self) -> bool {
        self.0.alive.load(Ordering::SeqCst)
    }
    fn busy(&self) -> bool {
        self.0.active.load(Ordering::SeqCst)
    }
    fn run(
        &self,
        _: RunRequest,
        _: WorkerRecord,
        _: Job,
        _: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>> {
        Box::pin(async move {
            self.0.active.store(true, Ordering::SeqCst);
            let _active = Active(&self.0.active);
            std::future::pending().await
        })
    }
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
    }
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async move {
            self.0.closed.fetch_add(1, Ordering::SeqCst);
            self.0.alive.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct FactoryStub(Arc<Workers>);
impl Factory for FactoryStub {
    fn instructions(&self, _: &Config, _: &WorkerRecord) -> anyhow::Result<String> {
        Ok("test instructions".into())
    }
    fn create(&self, _: WorkerSpec) -> Result<Arc<dyn Worker>, WorkerFailure> {
        self.0.created.fetch_add(1, Ordering::SeqCst);
        self.0.alive.store(true, Ordering::SeqCst);
        Ok(Arc::new(WorkerStub(self.0.clone())))
    }
}
struct Pages {
    requests: Mutex<Vec<PageRequest>>,
    responses: Mutex<VecDeque<Result<Value, HistoryFailure>>>,
    block: AtomicBool,
    active: AtomicBool,
    release: Semaphore,
}
impl History for Pages {
    fn page(&self, r: PageRequest) -> AdapterFuture<'_, Result<Value, HistoryFailure>> {
        Box::pin(async move {
            assert!(
                !self.active.swap(true, Ordering::SeqCst),
                "overlapping catch-up passes"
            );
            let _active = Active(&self.active);
            self.requests.lock().unwrap().push(r);
            if self.block.load(Ordering::SeqCst) {
                self.release.acquire().await.unwrap().forget();
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(json!({"ok":true,"messages":[]})))
        })
    }
}
struct WireState {
    status: watch::Sender<Status>,
    ready: Semaphore,
    active: AtomicBool,
    starts: AtomicUsize,
    receiver: Mutex<Option<Receiver>>,
    fatal: Notify,
    failure: Mutex<Option<socket::Failure>>,
    stubborn: AtomicBool,
}
#[derive(Clone)]
struct Wire(Arc<WireState>);
impl Connection for Wire {
    fn subscribe(&self) -> watch::Receiver<Status> {
        self.0.status.subscribe()
    }
    fn run(
        &self,
        mut stop: watch::Receiver<bool>,
    ) -> AdapterFuture<'_, Result<(), socket::Failure>> {
        Box::pin(async move {
            self.0.active.store(true, Ordering::SeqCst);
            self.0.starts.fetch_add(1, Ordering::SeqCst);
            let _active = Active(&self.0.active);
            let operation = async {
                self.0.ready.acquire().await.unwrap().forget();
                self.0.status.send_replace(Status::Connected);
                self.0.fatal.notified().await;
                Err(self
                    .0
                    .failure
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or(socket::Failure::Authentication))
            };
            let result = tokio::select! { result=operation=>result, _=async {
                loop {
                    if *stop.borrow_and_update() { break; }
                    if stop.changed().await.is_err(){break;}
                }
                if self.0.stubborn.load(Ordering::SeqCst) { std::future::pending::<()>().await; }
            }=>Ok(()) };
            self.0.status.send_replace(Status::Stopped);
            result
        })
    }
}
struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    clock: Arc<ReplayClock>,
    parent: Arc<ParentStub>,
    sink: Arc<Sink>,
    workers: Arc<Workers>,
    pages: Arc<Pages>,
    wire: Wire,
    service: Option<TestService>,
}
impl Harness {
    async fn new(observe: bool, options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let config = Arc::new(
            loader::parse(
                &format!(
                    r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local]
backends=["codex"]
[machines.local.workspaces]
project="{}"
[state]
path="{}"
control_socket="private/control.sock"
"#,
                    dir.path().join("project").display(),
                    dir.path().join("db").display()
                ),
                &dir.path().join("config.toml"),
                &LoadContext {
                    home: dir.path().into(),
                    runtime_dir: None,
                    uid: 1,
                    protected: vec![],
                },
            )
            .unwrap(),
        );
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = Arc::new(ReplayClock::new(10000.));
        let parent = Arc::new(ParentStub {
            calls: AtomicUsize::new(0),
            active: AtomicBool::new(false),
            block: AtomicBool::new(false),
            release: Semaphore::new(0),
            response: Mutex::new(json!({"reply":{"text":"Answer","status":"complete"}})),
        });
        let sink = Arc::new(Sink::default());
        let workers = Arc::new(Workers::default());
        let runtime = Runtime::start(
            store.clone(),
            config,
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(FactoryStub(workers.clone())),
                job_io: Arc::new(NoJobIo),
                machine_load: None,
            },
            clock.clone(),
            Arc::new(SequenceIds::default()),
            observe,
        )
        .await
        .unwrap();
        let pages = Arc::new(Pages {
            requests: Mutex::new(vec![]),
            responses: Mutex::new(VecDeque::new()),
            block: AtomicBool::new(false),
            active: AtomicBool::new(false),
            release: Semaphore::new(0),
        });
        let (status, _) = watch::channel(Status::Connecting);
        let wire = Wire(Arc::new(WireState {
            status,
            ready: Semaphore::new(0),
            active: AtomicBool::new(false),
            starts: AtomicUsize::new(0),
            receiver: Mutex::new(None),
            fatal: Notify::new(),
            failure: Mutex::new(None),
            stubborn: AtomicBool::new(false),
        }));
        let connection = wire.clone();
        let service = Service::new(
            runtime,
            pages.clone(),
            move |r| {
                *connection.0.receiver.lock().unwrap() = Some(r);
                Ok(connection)
            },
            options,
        )
        .unwrap();
        Self {
            _dir: dir,
            store,
            clock,
            parent,
            sink,
            workers,
            pages,
            wire,
            service: Some(service),
        }
    }
    fn start(
        &mut self,
    ) -> (
        watch::Sender<bool>,
        tokio::task::JoinHandle<Result<(), Failure>>,
    ) {
        let (stop, rx) = watch::channel(false);
        let service = self.service.take().unwrap();
        (stop, tokio::spawn(service.run(rx)))
    }
    fn connect(&self) {
        self.wire.0.ready.add_permits(1);
    }
    async fn intake(&self, ts: &str) {
        self.intake_thread(ts, None).await;
    }
    async fn intake_thread(&self, ts: &str, root: Option<&str>) {
        let receiver = self.wire.0.receiver.lock().unwrap().clone().unwrap();
        receiver.receive(&serde_json::to_vec(&json!({"type":"events_api","envelope_id":format!("envelope-{ts}"),"payload":{"type":"event_callback","team_id":"TTEAM","event_id":format!("event-{ts}"),"event":{"type":"message","channel":"CROOM","ts":ts,"thread_ts":root,"user":"UALICE","text":"<@UOWNER> help"}}})).unwrap()).await.unwrap();
    }
    async fn scalar(&self, sql: &'static str) -> i64 {
        self.store
            .call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
    async fn until(&self, sql: &'static str, want: i64) {
        let result = tokio::time::timeout(Duration::from_secs(4), async {
            while self.scalar(sql).await != want {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
        if result.is_err() {
            let actual = self.scalar(sql).await;
            let state: String = self.store.call(|c| Ok(c.query_row(
                "SELECT json_object('inbox',json((SELECT json_group_array(json_object('kind',kind,'state',state,'not_before',not_before,'attempts',attempts)) FROM thread_inbox)), 'outbox',json((SELECT json_group_array(json_object('state',state,'error',error)) FROM outbox)), 'obligations',json((SELECT json_group_array(json_object('state',state,'summary',summary)) FROM obligations)))",
                [], |r| r.get(0),
            )?)).await.unwrap();
            panic!("timed out waiting for {sql}: wanted {want}, got {actual}; state={state}");
        }
    }
}
fn options() -> Options {
    Options {
        pass_interval: Duration::from_millis(10),
        catchup_interval: Duration::from_secs(60),
        heartbeat_interval: Duration::from_millis(10),
        socket_stop_timeout: Duration::from_millis(100),
        ..Options::default()
    }
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(4), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
async fn finish(
    stop: watch::Sender<bool>,
    task: tokio::task::JoinHandle<Result<(), Failure>>,
) -> Result<(), Failure> {
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn startup_waits_for_authentication_then_drives_intake_and_delivery() {
    let mut h = Harness::new(false, options()).await;
    h.intake("9999.1").await;
    let (stop, task) = h.start();
    h.until("SELECT count(*) FROM runtime", 1).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert!(h.pages.requests.lock().unwrap().is_empty());
    h.connect();
    h.until("SELECT count(*) FROM obligations WHERE state='answered'", 1)
        .await;
    assert_eq!(h.sink.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(h.workers.created.load(Ordering::SeqCst), 0);
    finish(stop, task).await.unwrap();
    assert_eq!(h.scalar("SELECT count(*) FROM runtime WHERE slack_status='stopped' AND control_socket='' AND observe_only=0").await,1);
}
#[tokio::test]
async fn blocked_history_does_not_block_intake_parent_or_heartbeat_and_stop_cancels_it() {
    let mut h = Harness::new(false, options()).await;
    h.pages.block.store(true, Ordering::SeqCst);
    h.parent.block.store(true, Ordering::SeqCst);
    let (stop, task) = h.start();
    h.connect();
    wait_for(|| h.pages.active.load(Ordering::SeqCst)).await;
    h.intake("9999.1").await;
    wait_for(|| h.parent.active.load(Ordering::SeqCst)).await;
    h.intake("9999.2").await;
    h.clock.set(10001.);
    h.until("SELECT CAST(heartbeat_at AS INTEGER) FROM runtime", 10001)
        .await;
    assert_eq!(h.scalar("SELECT count(*) FROM obligations").await, 2);
    finish(stop, task).await.unwrap();
    wait_for(|| !h.parent.active.load(Ordering::SeqCst)).await;
    assert!(!h.pages.active.load(Ordering::SeqCst));
    assert!(!h.wire.0.active.load(Ordering::SeqCst));
    assert_eq!(
        h.scalar(
            "SELECT count(*) FROM replay_events WHERE kind='slack_history_call' AND complete=0"
        )
        .await,
        1
    );
    assert_eq!(h.sink.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn catchup_runs_periodically_while_disconnected_and_reconnect_hastens_it() {
    let mut o = options();
    o.catchup_interval = Duration::from_millis(200);
    let mut h = Harness::new(true, o).await;
    let (stop, task) = h.start();
    h.connect();
    h.until(
        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
        1,
    )
    .await;
    assert_eq!(h.pages.requests.lock().unwrap()[0].oldest, "6400.000000");
    h.wire.0.status.send_replace(Status::Reconnecting);
    h.clock.set(10010.);
    h.until(
        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
        2,
    )
    .await;
    // Existing watermark overlaps more than the recent window only when needed.
    assert_eq!(h.pages.requests.lock().unwrap()[1].oldest, "9110.000000");
    h.wire.0.status.send_replace(Status::Connected);
    h.until(
        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
        3,
    )
    .await;
    finish(stop, task).await.unwrap();
}
#[tokio::test]
async fn startup_history_keeps_the_outage_window_despite_new_live_intake() {
    let mut h = Harness::new(true, options()).await;
    let (stop, task) = h.start();
    h.until("SELECT count(*) FROM runtime", 1).await;
    h.intake("9999.1").await;
    h.pages.responses.lock().unwrap().push_back(Ok(json!({"ok":true,"messages":[{"type":"message","ts":"6500.1","user":"UALICE","text":"<@UOWNER> missed"}]})));
    h.connect();
    h.until(
        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
        1,
    )
    .await;
    assert_eq!(h.pages.requests.lock().unwrap()[0].oldest, "6400.000000");
    assert_eq!(h.scalar("SELECT count(*) FROM messages").await, 2);
    assert_eq!(h.scalar("SELECT count(*) FROM obligations").await, 2);
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.sink.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.workers.created.load(Ordering::SeqCst), 0);
    finish(stop, task).await.unwrap();
}
#[tokio::test]
async fn history_failures_are_visible_and_reconnect_honors_retry_after() {
    let mut o = options();
    o.catchup_interval = Duration::from_millis(30);
    let mut h = Harness::new(true, o).await;
    h.pages
        .responses
        .lock()
        .unwrap()
        .push_back(Err(HistoryFailure::RateLimited { retry_after: 1. }));
    let (stop, task) = h.start();
    h.connect();
    h.until(
        "SELECT count(*) FROM health_events WHERE kind='service_catchup_failed'",
        1,
    )
    .await;
    assert_eq!(h.scalar("SELECT count(*) FROM channel_watermarks").await, 0);
    h.wire.0.status.send_replace(Status::Reconnecting);
    tokio::time::sleep(Duration::from_millis(20)).await;
    h.wire.0.status.send_replace(Status::Connected);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.pages.requests.lock().unwrap().len(), 1);
    h.until("SELECT count(*) FROM channel_watermarks", 1).await;
    {
        let requests = h.pages.requests.lock().unwrap();
        assert_eq!(requests[0].oldest, requests[1].oldest);
    }
    finish(stop, task).await.unwrap();
}
#[tokio::test]
async fn owner_pause_controls_remain_available_during_history_and_mentions_do_not_resume() {
    let mut h = Harness::new(false, options()).await;
    h.intake("9999.1").await;
    let runtime = h.service.as_ref().unwrap().runtime();
    runtime
        .control(
            "TTEAM:CROOM:9999.1".into(),
            Control::Pause {
                reason: "Owner hold".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    h.pages.block.store(true, Ordering::SeqCst);
    let (stop, task) = h.start();
    h.connect();
    wait_for(|| h.pages.active.load(Ordering::SeqCst)).await;
    h.intake_thread("9999.2", Some("9999.1")).await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.scalar("SELECT count(*) FROM obligations").await, 2);
    assert!(runtime
        .control(
            "TTEAM:CROOM:9999.1".into(),
            Control::Resume,
            Authority::System
        )
        .await
        .is_err());
    runtime
        .control(
            "TTEAM:CROOM:9999.1".into(),
            Control::Resume,
            Authority::Owner,
        )
        .await
        .unwrap();
    h.until("SELECT count(*) FROM obligations WHERE state='answered'", 2)
        .await;
    finish(stop, task).await.unwrap();
}
#[tokio::test]
async fn fatal_socket_failure_stops_scheduling_and_closes_running_workers() {
    let mut h = Harness::new(false, options()).await;
    *h.parent.response.lock().unwrap() =
        json!({"delegations":[{"brief":"Run checks","machine":"local","workspace":"project"}]});
    let (_stop, task) = h.start();
    h.connect();
    h.intake("9999.1").await;
    wait_for(|| h.workers.active.load(Ordering::SeqCst)).await;
    *h.wire.0.failure.lock().unwrap() = Some(socket::Failure::LinkDisabled);
    h.wire.0.fatal.notify_one();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap(),
        Err(Failure::Socket {
            failure: socket::Failure::LinkDisabled
        })
    );
    assert!(h.workers.closed.load(Ordering::SeqCst) > 0);
    assert!(!h.workers.alive.load(Ordering::SeqCst));
    assert!(!h.workers.active.load(Ordering::SeqCst));
    assert_eq!(
        h.scalar("SELECT count(*) FROM jobs WHERE status='running'")
            .await,
        0
    );
    assert_eq!(
        h.scalar("SELECT count(*) FROM health_events WHERE kind='service_failed'")
            .await,
        1
    );
}
#[tokio::test]
async fn aborting_the_run_future_still_finishes_coordinator_cleanup() {
    let mut h = Harness::new(false, options()).await;
    h.parent.block.store(true, Ordering::SeqCst);
    let (_stop, task) = h.start();
    h.connect();
    h.intake("9999.1").await;
    wait_for(|| h.parent.active.load(Ordering::SeqCst)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    h.until(
        "SELECT count(*) FROM runtime WHERE slack_status='stopped'",
        1,
    )
    .await;
    wait_for(|| !h.parent.active.load(Ordering::SeqCst)).await;
    assert!(!h.wire.0.active.load(Ordering::SeqCst));
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='service_stop'")
            .await,
        1
    );
}
#[tokio::test]
async fn cancellation_during_delivery_preserves_ambiguity_for_restart() {
    let mut h = Harness::new(false, options()).await;
    h.sink.block.store(true, Ordering::SeqCst);
    let (stop, task) = h.start();
    h.connect();
    h.intake("9999.1").await;
    wait_for(|| h.sink.active.load(Ordering::SeqCst)).await;
    finish(stop, task).await.unwrap();
    assert!(!h.sink.active.load(Ordering::SeqCst));
    fridica::store::outbox::recover(&h.store, 10001.)
        .await
        .unwrap();
    assert_eq!(
        h.scalar("SELECT count(*) FROM outbox WHERE state='ambiguous'")
            .await,
        1
    );
    assert_eq!(
        h.scalar("SELECT count(*) FROM obligations WHERE state='answered'")
            .await,
        0
    );
    assert_eq!(h.sink.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn startup_storage_failure_prevents_external_io_and_shutdown_attempts_are_bounded() {
    let mut h = Harness::new(true, options()).await;
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_start BEFORE INSERT ON runtime BEGIN SELECT RAISE(ABORT,'secret failure'); END;")?;Ok(())}).await.unwrap();
    let (_stop, task) = h.start();
    h.connect();
    assert_eq!(task.await.unwrap(), Err(Failure::Storage));
    assert_eq!(h.wire.0.starts.load(Ordering::SeqCst), 0);
    assert!(h.pages.requests.lock().unwrap().is_empty());
    let mut h = Harness::new(true, options()).await;
    h.wire.0.stubborn.store(true, Ordering::SeqCst);
    let (stop, task) = h.start();
    h.connect();
    wait_for(|| h.wire.0.active.load(Ordering::SeqCst)).await;
    assert_eq!(finish(stop, task).await, Err(Failure::Shutdown));
    assert!(!h.wire.0.active.load(Ordering::SeqCst));
    assert_eq!(
        h.scalar("SELECT count(*) FROM health_events WHERE kind='service_socket_shutdown_failed'")
            .await,
        1
    );
}

#[tokio::test]
async fn runtime_failure_is_fatal_redacted_and_leaves_unclaimed_work_durable() {
    let mut h = Harness::new(false, options()).await;
    h.intake("9999.1").await;
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_claim BEFORE UPDATE ON thread_inbox WHEN NEW.state='processing' BEGIN SELECT RAISE(ABORT,'private storage detail'); END;")?;Ok(())}).await.unwrap();
    let (_stop, task) = h.start();
    h.connect();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap(),
        Err(Failure::Runtime)
    );
    assert!(!h.wire.0.active.load(Ordering::SeqCst));
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.scalar("SELECT count(*) FROM thread_inbox WHERE state='pending'")
            .await,
        1
    );
    assert_eq!(
        h.scalar(
            "SELECT count(*) FROM health_events WHERE details_json LIKE '%private storage detail%'"
        )
        .await,
        0
    );
}
#[tokio::test]
async fn startup_records_an_interrupted_service_and_stop_before_connect_does_no_work() {
    let mut h = Harness::new(false, options()).await;
    h.store.call(|c|{c.execute("INSERT INTO runtime(id,pid,started_at,heartbeat_at,slack_status,observe_only) VALUES(1,123,1,2,'connected',0)",[])?;Ok(())}).await.unwrap();
    let (stop, task) = h.start();
    h.until(
        "SELECT count(*) FROM health_events WHERE kind='service_interrupted'",
        1,
    )
    .await;
    drop(stop);
    task.await.unwrap().unwrap();
    assert!(h.pages.requests.lock().unwrap().is_empty());
    assert_eq!(h.parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(h.workers.created.load(Ordering::SeqCst), 0);
    assert_eq!(
        h.scalar("SELECT count(*) FROM runtime WHERE slack_status='stopped'")
            .await,
        1
    );
}
