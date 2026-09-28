use super::*;
use crate::{
    config::{loader, LoadContext},
    core::time::{ReplayClock, SequenceIds},
    store::Store,
};
use std::{collections::VecDeque, sync::Mutex as StdMutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
use tokio_tungstenite::{
    accept_hdr_async,
    tungstenite::handshake::server::{Request, Response},
};

type ServerSocket = WebSocketStream<TcpStream>;
struct TicketServer {
    base: Url,
    calls: Arc<StdMutex<Vec<(String, String)>>>,
    tickets: Arc<StdMutex<VecDeque<Value>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for TicketServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl TicketServer {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/api/", listener.local_addr().unwrap())).unwrap();
        let calls = Arc::new(StdMutex::new(vec![]));
        let tickets = Arc::new(StdMutex::new(VecDeque::<Value>::new()));
        let log = calls.clone();
        let queue = tickets.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = vec![];
                loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let headers = String::from_utf8(raw).unwrap();
                let path = headers.split_whitespace().nth(1).unwrap().to_string();
                let auth = headers
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("authorization:"))
                    .unwrap_or("")
                    .to_string();
                log.lock().unwrap().push((path.clone(), auth));
                let value = if path == "/api/auth.test" {
                    json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM"})
                } else if path.starts_with("/api/conversations.info?") {
                    json!({"ok":true,"channel":{"id":"CROOM","is_member":true}})
                } else if path.starts_with("/api/conversations.history?")
                    || path.starts_with("/api/conversations.replies?")
                {
                    json!({"ok":true,"messages":[]})
                } else if path == "/api/chat.postMessage" {
                    json!({"ok":true,"channel":"CROOM","ts":"200.1"})
                } else {
                    assert_eq!(path, "/api/apps.connections.open");
                    queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .expect("unexpected ticket request")
                };
                let body = serde_json::to_vec(&value).unwrap();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes()).await;
                let _ = stream.write_all(&body).await;
            }
        });
        Self {
            base,
            calls,
            tickets,
            task,
        }
    }
}
struct WsServer {
    url: Url,
    connections: mpsc::Receiver<ServerSocket>,
    headers: Arc<StdMutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for WsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl WsServer {
    // Tungstenite requires this handshake callback error type.
    #[allow(clippy::result_large_err)]
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = Url::parse(&format!(
            "ws://{}/link/?ticket=private-ticket",
            listener.local_addr().unwrap()
        ))
        .unwrap();
        let (send, connections) = mpsc::channel(4);
        let headers = Arc::new(StdMutex::new(vec![]));
        let log = headers.clone();
        let task = tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let log = log.clone();
                let socket =
                    accept_hdr_async(stream, move |request: &Request, response: Response| {
                        log.lock().unwrap().push(format!("{:?}", request.headers()));
                        Ok(response)
                    })
                    .await
                    .unwrap();
                if send.send(socket).await.is_err() {
                    break;
                }
            }
        });
        Self {
            url,
            connections,
            headers,
            task,
        }
    }
    async fn accept(&mut self) -> ServerSocket {
        tokio::time::timeout(Duration::from_secs(3), self.connections.recv())
            .await
            .unwrap()
            .unwrap()
    }
}
struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    clock: Arc<ReplayClock>,
    http: TicketServer,
    ws: WsServer,
    mode: Arc<SocketMode>,
}
impl Harness {
    async fn new(options: Options) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let source = format!(
            r#"[owner]
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
"#,
            dir.path().join("project").display(),
            dir.path().join("db").display()
        );
        let config = Arc::new(
            loader::parse(
                &source,
                &dir.path().join("config.toml"),
                &LoadContext {
                    home: dir.path().into(),
                    uid: 1,
                    runtime_dir: None,
                    protected: vec![],
                },
            )
            .unwrap(),
        );
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = Arc::new(ReplayClock::new(1000.));
        let http = TicketServer::new().await;
        let ws = WsServer::new().await;
        let mut web = WebClient::new(
            config.clone(),
            store.clone(),
            clock.clone(),
            "xoxp-owner-secret".into(),
            Duration::from_secs(2),
        )
        .unwrap();
        web.test_endpoint(http.base.clone());
        let receiver = Receiver::new(
            store.clone(),
            config,
            clock.clone(),
            Arc::new(SequenceIds::default()),
        );
        let mut mode =
            SocketMode::new(Arc::new(web), receiver, "xapp-app-secret".into(), options).unwrap();
        mode.test_origin = Some(ws.url.clone());
        Self {
            _dir: dir,
            store,
            clock,
            http,
            ws,
            mode: Arc::new(mode),
        }
    }
    fn ticket(&self) {
        self.http
            .tickets
            .lock()
            .unwrap()
            .push_back(json!({"ok":true,"url":self.ws.url.as_str()}));
    }
    fn start(&self) -> (watch::Sender<bool>, tokio::task::JoinHandle<Result<()>>) {
        let (stop, rx) = watch::channel(false);
        let mode = self.mode.clone();
        let task = tokio::spawn(async move { mode.run(rx).await });
        (stop, task)
    }
    async fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.store
            .call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
}
fn options() -> Options {
    Options {
        backoff_min: Duration::from_millis(20),
        backoff_max: Duration::from_millis(80),
        ..Default::default()
    }
}
async fn text(socket: &mut ServerSocket, value: Value) {
    socket
        .send(Message::Text(value.to_string().into()))
        .await
        .unwrap();
}
fn event() -> Value {
    json!({"type":"events_api","envelope_id":"env1","payload":{"type":"event_callback","event_id":"Ev1","team_id":"TTEAM","event":{"type":"message","channel":"CROOM","user":"UALICE","ts":"100.1","text":"<@UOWNER> help"}}})
}
async fn next(socket: &mut ServerSocket) -> Message {
    tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn wait_for_status(mode: &SocketMode, status: Status) {
    let mut states = mode.subscribe();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if *states.borrow_and_update() == status {
                break;
            }
            states.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn authenticated_ticket_and_ack_follow_durable_intake_without_disclosing_credentials() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    let (stop, task) = h.start();
    let mut socket = h.ws.accept().await;
    text(&mut socket, json!({"type":"hello"})).await;
    wait_for_status(&h.mode, Status::Connected).await;
    text(&mut socket, event()).await;
    let ack = next(&mut socket).await.into_text().unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&ack).unwrap(),
        json!({"envelope_id":"env1"})
    );
    assert_eq!(h.count("messages").await, 1);
    assert_eq!(h.count("thread_inbox").await, 1);
    assert_eq!(h.count("obligations").await, 1);
    text(&mut socket, event()).await;
    next(&mut socket).await;
    assert_eq!(h.count("messages").await, 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(*h.mode.subscribe().borrow(), Status::Stopped);
    let rows: Vec<(String, String)> = h
        .store
        .call(|c| {
            Ok(
                c.prepare("SELECT kind,payload_json FROM replay_events ORDER BY seq")?
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
        .await
        .unwrap();
    let kinds: Vec<_> = rows.iter().map(|r| r.0.as_str()).collect();
    assert!(
        kinds.iter().position(|v| *v == "intake").unwrap()
            < kinds.iter().position(|v| *v == "slack_ack_call").unwrap()
    );
    let ledger = format!("{rows:?}");
    for secret in ["xapp-app-secret", "xoxp-owner-secret", "private-ticket"] {
        assert!(!ledger.contains(secret));
    }
    let calls = h.http.calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls[0].1.contains("xoxp-owner-secret"));
    assert!(calls[2].1.contains("xapp-app-secret"));
    assert_eq!(calls[2].0, "/api/apps.connections.open");
    assert!(!h.ws.headers.lock().unwrap()[0].contains("authorization"));
}
#[tokio::test]
async fn refresh_gets_a_new_ticket_and_redelivery_keeps_one_obligation() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.ticket();
    let (stop, task) = h.start();
    let mut first = h.ws.accept().await;
    text(&mut first, json!({"type":"hello"})).await;
    text(&mut first, event()).await;
    next(&mut first).await;
    text(&mut first, json!({"type":"disconnect","reason":"warning"})).await;
    text(&mut first, event()).await;
    next(&mut first).await;
    assert_eq!(h.http.calls.lock().unwrap().len(), 3);
    text(
        &mut first,
        json!({"type":"disconnect","reason":"refresh_requested"}),
    )
    .await;
    let mut second = h.ws.accept().await;
    text(&mut second, json!({"type":"hello"})).await;
    text(&mut second, event()).await;
    next(&mut second).await;
    assert_eq!(h.count("obligations").await, 1);
    assert_eq!(h.http.calls.lock().unwrap().len(), 4);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}
#[tokio::test]
async fn intake_failure_never_acknowledges_and_reconnect_recovers_the_retry() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.ticket();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_intake BEFORE INSERT ON obligations BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    let (stop, task) = h.start();
    let mut first = h.ws.accept().await;
    text(&mut first, json!({"type":"hello"})).await;
    text(&mut first, event()).await;
    let closed = tokio::time::timeout(Duration::from_secs(2), first.next())
        .await
        .unwrap();
    assert!(closed.is_none() || closed.unwrap().is_err());
    assert_eq!(h.count("messages").await, 0);
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_intake")?;
            Ok(())
        })
        .await
        .unwrap();
    let mut second = h.ws.accept().await;
    text(&mut second, json!({"type":"hello"})).await;
    text(&mut second, event()).await;
    next(&mut second).await;
    assert_eq!(h.count("obligations").await, 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert!(h.count("health_events").await >= 1);
}
#[tokio::test]
async fn failed_ack_intent_leaves_intake_durable_and_sends_nothing() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_ack BEFORE INSERT ON replay_events WHEN NEW.kind='slack_ack_call' BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    let (_stop, task) = h.start();
    let mut socket = h.ws.accept().await;
    text(&mut socket, json!({"type":"hello"})).await;
    text(&mut socket, event()).await;
    assert_eq!(task.await.unwrap(), Err(Failure::Storage));
    assert_eq!(h.count("messages").await, 1);
    assert_eq!(h.count("obligations").await, 1);
    let result = socket.next().await;
    assert!(result.is_none() || result.unwrap().is_err());
}
#[tokio::test]
async fn pings_are_answered_and_matching_pongs_keep_the_connection_alive() {
    let mut opts = options();
    opts.ping_interval = Duration::from_millis(30);
    opts.pong_timeout = Duration::from_secs(1);
    let mut h = Harness::new(opts).await;
    h.ticket();
    let (stop, task) = h.start();
    let mut socket = h.ws.accept().await;
    text(&mut socket, json!({"type":"hello"})).await;
    socket
        .send(Message::Ping(b"server-ping".as_slice().into()))
        .await
        .unwrap();
    loop {
        match next(&mut socket).await {
            Message::Pong(bytes) => {
                assert_eq!(&bytes[..], b"server-ping");
                break;
            }
            Message::Ping(_) => socket.flush().await.unwrap(),
            other => panic!("{other:?}"),
        }
    }
    for _ in 0..2 {
        assert!(matches!(next(&mut socket).await, Message::Ping(_)));
        socket.flush().await.unwrap();
    }
    text(&mut socket, event()).await;
    loop {
        match next(&mut socket).await {
            Message::Text(_) => break,
            Message::Ping(_) => socket.flush().await.unwrap(),
            other => panic!("{other:?}"),
        }
    }
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(h.http.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn invalid_frames_and_missing_hello_reconnect_with_visible_health() {
    for mode in [
        "binary",
        "oversized",
        "malformed",
        "before_hello",
        "no_hello",
    ] {
        let mut opts = options();
        opts.hello_timeout = Duration::from_millis(200);
        let mut h = Harness::new(opts).await;
        h.ticket();
        h.ticket();
        let (stop, task) = h.start();
        let mut first = h.ws.accept().await;
        if !matches!(mode, "before_hello" | "no_hello") {
            text(&mut first, json!({"type":"hello"})).await;
        }
        match mode {
            "binary" => first
                .send(Message::Binary(vec![1, 2].into()))
                .await
                .unwrap(),
            "oversized" => {
                let _ = first
                    .send(Message::Text("x".repeat(ENVELOPE_LIMIT + 1).into()))
                    .await;
            }
            "malformed" => first.send(Message::Text("invalid".into())).await.unwrap(),
            "before_hello" => text(&mut first, event()).await,
            _ => (),
        }
        let mut second = h.ws.accept().await;
        text(&mut second, json!({"type":"hello"})).await;
        wait_for_status(&h.mode, Status::Connected).await;
        assert_eq!(h.count("messages").await, 0);
        assert!(h.count("health_events").await >= 1);
        stop.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
#[tokio::test]
async fn link_disabled_and_authentication_errors_stop_without_reconnect_loops() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    let (_stop, task) = h.start();
    let mut socket = h.ws.accept().await;
    text(&mut socket, json!({"type":"hello"})).await;
    text(
        &mut socket,
        json!({"type":"disconnect","reason":"link_disabled"}),
    )
    .await;
    assert_eq!(task.await.unwrap(), Err(Failure::LinkDisabled));
    assert_eq!(h.http.calls.lock().unwrap().len(), 3);
    let h = Harness::new(options()).await;
    h.http
        .tickets
        .lock()
        .unwrap()
        .push_back(json!({"ok":false,"error":"invalid_auth"}));
    let (_stop, task) = h.start();
    assert_eq!(task.await.unwrap(), Err(Failure::Authentication));
    assert_eq!(h.http.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn stop_interrupts_backoff_and_second_run_is_rejected() {
    let mut opts = options();
    opts.backoff_min = Duration::from_secs(60);
    opts.backoff_max = Duration::from_secs(60);
    let h = Harness::new(opts).await;
    h.http
        .tickets
        .lock()
        .unwrap()
        .push_back(json!({"ok":false,"error":"internal_error"}));
    let (stop, task) = h.start();
    wait_for_status(&h.mode, Status::Reconnecting).await;
    let (_other, rx) = watch::channel(false);
    assert_eq!(h.mode.run(rx).await, Err(Failure::AlreadyRunning));
    drop(stop);
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(h.http.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn ticket_validation_rejects_untrusted_origins_without_attempting_connection() {
    let h = Harness::new(options()).await;
    for bad in [
        "ws://wss.slack.com/link/?ticket=x",
        "wss://evil.test/link/?ticket=x",
        "wss://slack.com.evil.test/link/?ticket=x",
        "wss://user@wss.slack.com/link/?ticket=x",
        "wss://wss.slack.com:444/link/?ticket=x",
        "wss://wss.slack.com/link/?ticket=x#bad",
        "wss://wss.slack.com/link/",
        "wss://wss.slack.com/link/?ticket=x\n",
    ] {
        assert_eq!(h.mode.url(bad), Err(Failure::Configuration), "{bad}");
    }
    assert!(h
        .mode
        .url("wss://wss-primary.slack.com/link/?ticket=x&app_id=A1")
        .is_ok());
}

#[tokio::test]
async fn a_missing_pong_forces_a_fresh_connection() {
    let mut opts = options();
    opts.ping_interval = Duration::from_millis(20);
    opts.pong_timeout = Duration::from_millis(100);
    let mut h = Harness::new(opts).await;
    h.ticket();
    h.ticket();
    let (stop, task) = h.start();
    let mut first = h.ws.accept().await;
    text(&mut first, json!({"type":"hello"})).await;
    // Do not poll the server stream: tungstenite must not flush its auto-Pong.
    let mut second = h.ws.accept().await;
    text(&mut second, json!({"type":"hello"})).await;
    wait_for_status(&h.mode, Status::Connected).await;
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(h.http.calls.lock().unwrap().len(), 4);
    assert!(h.count("health_events").await >= 1);
}
#[tokio::test]
async fn a_failed_ack_result_cannot_erase_the_committed_message_or_hide_uncertainty() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_result BEFORE INSERT ON replay_events WHEN NEW.kind='slack_ack_result' BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    let (_stop, task) = h.start();
    let mut socket = h.ws.accept().await;
    text(&mut socket, json!({"type":"hello"})).await;
    text(&mut socket, event()).await;
    assert!(matches!(next(&mut socket).await, Message::Text(_)));
    assert_eq!(task.await.unwrap(), Err(Failure::Storage));
    assert_eq!(h.count("messages").await, 1);
    let incomplete: i64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM replay_events WHERE kind='slack_ack_call' AND complete=0",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(incomplete, 1);
}
#[tokio::test]
async fn aborting_a_connection_closes_it_and_next_run_reports_interruption() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.ticket();
    let (stop, task) = h.start();
    let mut first = h.ws.accept().await;
    text(&mut first, json!({"type":"hello"})).await;
    wait_for_status(&h.mode, Status::Connected).await;
    task.abort();
    let _ = task.await;
    assert_eq!(*h.mode.subscribe().borrow(), Status::Stopped);
    let result = tokio::time::timeout(Duration::from_secs(2), first.next())
        .await
        .unwrap();
    assert!(result.is_none() || result.unwrap().is_err());
    drop(stop);
    let (stop, task) = h.start();
    let mut second = h.ws.accept().await;
    text(&mut second, json!({"type":"hello"})).await;
    wait_for_status(&h.mode, Status::Connected).await;
    let interrupted: i64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM health_events WHERE kind='slack_socket_interrupted'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(interrupted, 1);
    stop.send(true).unwrap();
    task.await.unwrap().unwrap();
}
#[tokio::test]
async fn reconnect_backoff_increases_and_bad_ticket_errors_do_not_disclose_the_ticket() {
    let h = Harness::new(options()).await;
    for _ in 0..3 {
        h.http
            .tickets
            .lock()
            .unwrap()
            .push_back(json!({"ok":false,"error":"internal_error"}));
    }
    h.http
        .tickets
        .lock()
        .unwrap()
        .push_back(json!({"ok":true,"url":"wss://evil.test/link/?ticket=private-ticket"}));
    let (_stop, task) = h.start();
    assert_eq!(task.await.unwrap(), Err(Failure::Configuration));
    let rows:Vec<String>=h.store.call(|c|Ok(c.prepare("SELECT payload_json FROM replay_events WHERE kind='slack_socket_backoff' ORDER BY seq")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?)).await.unwrap();
    let delays: Vec<f64> = rows
        .iter()
        .map(|s| {
            serde_json::from_str::<Value>(s).unwrap()["seconds"]
                .as_f64()
                .unwrap()
        })
        .collect();
    assert_eq!(delays, vec![0.02, 0.04, 0.08]);
    let raw: String = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT group_concat(payload_json) FROM replay_events",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(!raw.contains("private-ticket"));
    assert!(!raw.contains("xapp-app-secret"));
}
#[tokio::test]
async fn connect_recording_failure_prevents_the_websocket_handshake() {
    let mut h = Harness::new(options()).await;
    h.ticket();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_connect BEFORE INSERT ON replay_events WHEN NEW.kind='slack_socket_connect' BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    let (_stop, task) = h.start();
    assert_eq!(task.await.unwrap(), Err(Failure::Storage));
    assert!(h.ws.connections.try_recv().is_err());
    assert_eq!(h.count("messages").await, 0);
}

struct ServiceParent;
impl crate::core::parent::Parent for ServiceParent {
    fn decide(
        &self,
        request: crate::core::parent::ParentRequest,
    ) -> crate::core::delivery::AdapterFuture<
        '_,
        std::result::Result<Value, crate::core::parent::ParentFailure>,
    > {
        Box::pin(async move {
            let answers: Vec<_> = request
                .obligations
                .iter()
                .map(|o| o["id"].clone())
                .collect();
            Ok(json!({"reply":{"text":"Answer","status":"complete","answers":answers}}))
        })
    }
}
struct NoServiceWorkers;
impl crate::workers::protocol::Factory for NoServiceWorkers {
    fn instructions(
        &self,
        _: &crate::config::Config,
        _: &crate::core::worker::WorkerRecord,
    ) -> anyhow::Result<String> {
        panic!("this service scenario must not prepare a worker")
    }
    fn create(
        &self,
        _: crate::workers::protocol::WorkerSpec,
    ) -> std::result::Result<
        Arc<dyn crate::workers::protocol::Worker>,
        crate::core::worker::WorkerFailure,
    > {
        panic!("this service scenario must not start a worker")
    }
}
#[tokio::test]
async fn service_composes_real_socket_history_and_delivery_across_a_reconnect() {
    use crate::threads::{
        runtime::{Adapters, Runtime},
        service::{Options as ServiceOptions, Service},
    };
    let mut h = Harness::new(options()).await;
    h.clock.set(10000.);
    let receiver = h.mode.receiver.clone();
    let web = h.mode.web.clone();
    let runtime = Runtime::start(
        h.store.clone(),
        receiver.config.clone(),
        Adapters {
            parent: Arc::new(ServiceParent),
            delivery: web.clone(),
            workers: Arc::new(NoServiceWorkers),
            job_io: Arc::new(crate::workers::protocol::NoJobIo),
        },
        receiver.clock.clone(),
        receiver.ids.clone(),
        false,
    )
    .await
    .unwrap();
    let origin = h.ws.url.clone();
    let service = Service::new(
        runtime,
        web.clone(),
        move |receiver| {
            let mut mode = SocketMode::new(web, receiver, "xapp-app-secret".into(), options())?;
            mode.test_origin = Some(origin);
            Ok(mode)
        },
        ServiceOptions {
            pass_interval: Duration::from_millis(10),
            ..ServiceOptions::default()
        },
    )
    .unwrap();
    h.ticket();
    h.ticket();
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(service.run(rx));
    let mut first = h.ws.accept().await;
    text(&mut first, json!({"type":"hello"})).await;
    text(&mut first, event()).await;
    assert_eq!(
        serde_json::from_str::<Value>(&next(&mut first).await.into_text().unwrap()).unwrap(),
        json!({"envelope_id":"env1"})
    );
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let done: bool = h
                .store
                .call(|c| {
                    Ok(c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM obligations WHERE state='answered')",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            if done {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    text(
        &mut first,
        json!({"type":"disconnect","reason":"refresh_requested"}),
    )
    .await;
    let mut second = h.ws.accept().await;
    text(&mut second, json!({"type":"hello"})).await;
    text(&mut second, event()).await;
    let _ = next(&mut second).await;
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let passes: i64 = h
                .store
                .call(|c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM replay_events WHERE kind='service_catchup'",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            if passes >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    stop.send_replace(true);
    tokio::time::timeout(Duration::from_secs(4), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(h.count("obligations").await, 1);
    assert_eq!(h.count("outbox").await, 1);
    let calls = h.http.calls.lock().unwrap();
    assert_eq!(
        calls
            .iter()
            .filter(|(path, _)| path == "/api/chat.postMessage")
            .count(),
        1
    );
    assert_eq!(
        calls
            .iter()
            .filter(|(path, _)| path == "/api/apps.connections.open")
            .count(),
        2
    );
    assert!(
        calls
            .iter()
            .filter(|(path, _)| path.starts_with("/api/conversations.history?"))
            .count()
            >= 2
    );
}
