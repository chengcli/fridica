use super::*;
use crate::{
    config::{loader, LoadContext},
    core::{
        delivery::Post,
        time::{ReplayClock, SequenceIds},
    },
    slack::{catchup::Catchup, outbox::Dispatcher, receiver::Receiver},
    store::outbox,
};
use std::{collections::VecDeque, sync::Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    disconnect: bool,
    hang: bool,
    chunked: bool,
    truncated: bool,
}
impl Reply {
    fn json(value: Value) -> Self {
        Self {
            status: 200,
            headers: vec![],
            body: serde_json::to_vec(&value).unwrap(),
            disconnect: false,
            hang: false,
            chunked: false,
            truncated: false,
        }
    }
}
#[derive(Debug)]
struct Received {
    path: String,
    headers: String,
    body: Vec<u8>,
}
struct Server {
    base: Url,
    calls: Arc<Mutex<Vec<Received>>>,
    responses: Arc<Mutex<VecDeque<Reply>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = Url::parse(&format!("http://{}/api/", listener.local_addr().unwrap())).unwrap();
        let calls = Arc::new(Mutex::new(vec![]));
        let responses = Arc::new(Mutex::new(VecDeque::<Reply>::new()));
        let log = calls.clone();
        let queue = responses.clone();
        let task = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut raw = vec![];
                let boundary;
                loop {
                    let mut buf = [0u8; 1024];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                        boundary = end + 4;
                        break;
                    }
                    assert!(raw.len() < 65536);
                }
                let headers = String::from_utf8(raw[..boundary].to_vec()).unwrap();
                let length = headers
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                while raw.len() - boundary < length {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                }
                log.lock().unwrap().push(Received {
                    path: headers.split_whitespace().nth(1).unwrap().into(),
                    headers,
                    body: raw[boundary..].to_vec(),
                });
                let reply = queue
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected HTTP request");
                if reply.hang {
                    std::future::pending::<()>().await;
                }
                if reply.disconnect {
                    continue;
                }
                let length = if reply.chunked {
                    "Transfer-Encoding: chunked".into()
                } else {
                    format!(
                        "Content-Length: {}",
                        reply.body.len() + if reply.truncated { 10 } else { 0 }
                    )
                };
                let mut headers = format!(
                    "HTTP/1.1 {} TEST\r\n{}\r\nConnection: close\r\n",
                    reply.status, length
                );
                for (key, value) in reply.headers {
                    headers += &format!("{key}: {value}\r\n");
                }
                headers += "\r\n";
                if stream.write_all(headers.as_bytes()).await.is_ok() {
                    if reply.chunked {
                        let _ = stream
                            .write_all(format!("{:x}\r\n", reply.body.len()).as_bytes())
                            .await;
                        let _ = stream.write_all(&reply.body).await;
                        let _ = stream.write_all(b"\r\n0\r\n\r\n").await;
                    } else {
                        let _ = stream.write_all(&reply.body).await;
                    }
                }
            }
        });
        Self {
            base,
            calls,
            responses,
            task,
        }
    }
    fn add(&self, reply: Reply) {
        self.responses.lock().unwrap().push_back(reply);
    }
    fn json(&self, value: Value) {
        self.add(Reply::json(value));
    }
    fn validate(&self) {
        let mut auth = Reply::json(json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM"}));
        auth.headers.push((
            "x-oauth-scopes".into(),
            "chat:write, files:read, files:write".into(),
        ));
        self.add(auth);
        self.json(json!({"ok":true,"channel":{"id":"CROOM","is_member":true}}));
    }
}
struct Harness {
    _dir: tempfile::TempDir,
    server: Server,
    web: Arc<WebClient>,
    store: Store,
    clock: Arc<ReplayClock>,
}
impl Harness {
    async fn new(timeout: Duration) -> Self {
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
        let server = Server::new().await;
        let mut web = WebClient::new(
            config,
            store.clone(),
            clock.clone(),
            "xoxp-private-test-secret".into(),
            timeout,
        )
        .unwrap();
        web.base = server.base.clone();
        web.upload_origin = Some(server.base.clone());
        Self {
            _dir: dir,
            server,
            web: Arc::new(web),
            store,
            clock,
        }
    }
    async fn ready(&self) {
        self.server.validate();
        self.web.validate().await.unwrap();
    }
    async fn ledger(&self) -> String {
        self.store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT COALESCE(group_concat(payload_json),'') FROM replay_events",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }
    fn dispatcher(&self) -> Dispatcher<WebClient> {
        Dispatcher {
            store: self.store.clone(),
            delivery: self.web.clone(),
            clock: self.clock.clone(),
            owner: "UOWNER".into(),
            observe_only: false,
            timeout: Duration::from_secs(2),
        }
    }
}
fn post() -> ClaimedPost {
    ClaimedPost {
        id: 1,
        attempt: 1,
        post: Post {
            idem_key: "reply1".into(),
            session_id: "TTEAM:CROOM:100.1".into(),
            kind: "reply".into(),
            channel: "CROOM".into(),
            thread_ts: Some("100.1".into()),
            text: "hello".into(),
            meta: Some(
                json!({"owner":"UOWNER","session":"TTEAM:CROOM:100.1","turn":3,"status":"waiting","kind":"report","worker":"w1","v":2}),
            ),
            filename: String::new(),
            blob: None,
            after: String::new(),
        },
    }
}
#[tokio::test]
async fn owner_membership_validation_and_message_delivery_use_fixed_authenticated_requests() {
    let h = Harness::new(Duration::from_secs(2)).await;
    assert!(matches!(
        h.web.send(post()).await,
        DeliveryOutcome::Rejected { .. }
    ));
    assert!(h.server.calls.lock().unwrap().is_empty());
    h.server.validate();
    let identity = h.web.validate().await.unwrap();
    assert!(identity.scopes.unwrap().contains("files:read"));
    h.server
        .json(json!({"ok":true,"ts":"200.1","channel":"CROOM"}));
    assert_eq!(
        h.web.send(post()).await,
        DeliveryOutcome::Sent {
            reference: "200.1".into()
        }
    );
    {
        let calls = h.server.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].path, "/api/auth.test");
        assert!(calls[1]
            .path
            .starts_with("/api/conversations.info?channel=CROOM"));
        assert!(calls[1].headers.starts_with("GET "));
        assert_eq!(calls[2].path, "/api/chat.postMessage");
        for call in calls.iter() {
            assert!(call
                .headers
                .contains("authorization: Bearer xoxp-private-test-secret"));
        }
        let body: Value = serde_json::from_slice(&calls[2].body).unwrap();
        assert_eq!(body["unfurl_links"], false);
        assert_eq!(body["unfurl_media"], false);
        assert_eq!(body["thread_ts"], "100.1");
        assert_eq!(
            body["metadata"]["event_payload"]["task_id"],
            "TTEAM:CROOM:100.1"
        );
    }
    let ledger = h.ledger().await;
    assert!(!ledger.contains("private-test-secret"));
    assert!(ledger.contains("200.1"));
}
#[tokio::test]
async fn wrong_identity_bot_and_missing_membership_never_enable_writes() {
    for (auth, member) in [
        (json!({"ok":true,"user_id":"OTHER","team_id":"TTEAM"}), true),
        (
            json!({"ok":true,"user_id":"UOWNER","team_id":"OTHER"}),
            true,
        ),
        (
            json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","bot_id":"B1"}),
            true,
        ),
        (
            json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM"}),
            false,
        ),
    ] {
        let h = Harness::new(Duration::from_secs(1)).await;
        h.server.json(auth);
        if !member {
            h.server
                .json(json!({"ok":true,"channel":{"id":"CROOM","is_member":false}}));
        }
        assert!(h.web.validate().await.is_err());
        let n = h.server.calls.lock().unwrap().len();
        assert!(matches!(
            h.web.send(post()).await,
            DeliveryOutcome::Rejected { .. }
        ));
        assert_eq!(h.server.calls.lock().unwrap().len(), n);
    }
    let h = Harness::new(Duration::from_secs(1)).await;
    h.ready().await;
    let mut claim = post();
    claim.post.channel = "OTHER".into();
    assert!(matches!(
        h.web.send(claim).await,
        DeliveryOutcome::Rejected { .. }
    ));
    assert_eq!(h.server.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn status_envelopes_and_rate_limits_map_without_automatic_retries() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    let cases = [
        (
            200,
            json!({"ok":false,"error":"channel_not_found"}),
            "rejected",
        ),
        (
            500,
            json!({"ok":false,"error":"channel_not_found"}),
            "ambiguous",
        ),
        (200, json!({"ok":false,"error":"fatal_error"}), "ambiguous"),
        (200, json!({"ok":true}), "ambiguous"),
        (200, json!({"ok":true,"ts":"bad"}), "ambiguous"),
        (
            200,
            json!({"ok":true,"ts":"200.1","channel":"WRONG"}),
            "ambiguous",
        ),
        (
            200,
            json!({"ok":false,"error":"private error xoxp-private-test-secret"}),
            "rejected",
        ),
        (401, json!({}), "rejected"),
        (302, json!({}), "ambiguous"),
    ];
    for (status, body, expected) in cases {
        let mut reply = Reply::json(body);
        reply.status = status;
        reply.headers.push((
            "Location".into(),
            h.server.base.join("redirected").unwrap().to_string(),
        ));
        h.server.add(reply);
        let result = h.web.send(post()).await;
        assert_eq!(serde_json::to_value(&result).unwrap()["outcome"], expected);
        assert!(!format!("{result:?}").contains("private"));
    }
    for (header, expected) in [("7", 7.), ("NaN", 30.), ("invalid", 30.), ("-1", -1.)] {
        let mut reply = Reply::json(json!({"ok":false}));
        reply.status = 429;
        reply.headers.push(("Retry-After".into(), header.into()));
        h.server.add(reply);
        assert_eq!(
            h.web.send(post()).await,
            DeliveryOutcome::RateLimited {
                retry_after: expected
            }
        );
    }
    assert_eq!(h.server.calls.lock().unwrap().len(), 15);
    assert!(!h.ledger().await.contains("private-test-secret"));
}
#[tokio::test]
async fn uploads_send_bytes_without_credentials_and_require_matching_confirmation() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    let url = h
        .server
        .base
        .join("/upload/v1/private-upload-capability")
        .unwrap()
        .to_string();
    h.server
        .json(json!({"ok":true,"file_id":"F123","upload_url":url}));
    let mut reply = Reply::json(json!({}));
    reply.body = b"OK - 5".to_vec();
    h.server.add(reply);
    h.server.json(json!({"ok":true,"files":[{"id":"F123"}]}));
    let mut claim = post();
    claim.post.kind = "upload".into();
    claim.post.filename = "report.md".into();
    claim.post.blob = Some(b"hello".to_vec());
    assert_eq!(
        h.web.send(claim).await,
        DeliveryOutcome::Sent {
            reference: "F123".into()
        }
    );
    {
        let calls = h.server.calls.lock().unwrap();
        assert_eq!(calls.len(), 5);
        assert_eq!(calls[2].path, "/api/files.getUploadURLExternal");
        assert!(!calls[3].headers.contains("authorization"));
        assert_eq!(calls[3].body, b"hello");
        assert_eq!(calls[4].path, "/api/files.completeUploadExternal");
        let body: Value = serde_json::from_slice(&calls[4].body).unwrap();
        assert_eq!(
            body,
            json!({"channel_id":"CROOM","thread_ts":"100.1","files":[{"id":"F123","title":"report.md"}]})
        );
    }
    let ledger = h.ledger().await;
    assert!(!ledger.contains("private-upload-capability"));
    assert!(!ledger.contains("private-test-secret"));
}
#[tokio::test]
async fn upload_destinations_and_incomplete_completion_fail_closed() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    for url in [
        "http://files.slack.com/upload/v1/a",
        "https://files.slack.com.evil.test/upload/a",
        "https://files.slack.com@evil.test/upload/a",
        "https://files.slack.com:443/upload/a",
        "https://files.slack.com/a",
        "https://files.slack.com/upload/../../bad",
    ] {
        h.server
            .json(json!({"ok":true,"file_id":"F123","upload_url":url}));
        let mut claim = post();
        claim.post.kind = "upload".into();
        claim.post.filename = "x".into();
        claim.post.blob = Some(vec![1]);
        assert!(matches!(
            h.web.send(claim).await,
            DeliveryOutcome::Ambiguous { .. }
        ));
    }
    assert_eq!(h.server.calls.lock().unwrap().len(), 8);
    for confirmed in [
        json!({"ok":true}),
        json!({"ok":true,"files":[{"id":"FOTHER"}]}),
    ] {
        h.server.json(json!({"ok":true,"file_id":"F123","upload_url":h.server.base.join("/upload/v1/test").unwrap().as_str()}));
        h.server.add(Reply::json(json!({})));
        h.server.json(confirmed);
        let mut claim = post();
        claim.post.kind = "upload".into();
        claim.post.filename = "x".into();
        claim.post.blob = Some(vec![1]);
        assert_eq!(
            h.web.send(claim).await,
            DeliveryOutcome::Ambiguous {
                code: "unconfirmed_file".into()
            }
        );
    }
}
#[tokio::test]
async fn disconnect_timeout_malformed_and_oversized_responses_are_ambiguous() {
    for mode in ["disconnect", "timeout", "malformed", "oversized"] {
        let h = Harness::new(Duration::from_millis(250)).await;
        h.ready().await;
        let mut reply = Reply::json(json!({}));
        match mode {
            "disconnect" => reply.disconnect = true,
            "timeout" => reply.hang = true,
            "malformed" => reply.body = b"not json".to_vec(),
            _ => reply.body = vec![b'x'; ENVELOPE_LIMIT + 1],
        };
        h.server.add(reply);
        assert!(matches!(
            h.web.send(post()).await,
            DeliveryOutcome::Ambiguous { .. }
        ));
        assert_eq!(h.server.calls.lock().unwrap().len(), 3);
        if mode != "malformed" {
            let incomplete: i64 = h
                .store
                .call(|c| {
                    Ok(c.query_row(
                        "SELECT count(*) FROM replay_events WHERE complete=0",
                        [],
                        |r| r.get(0),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(incomplete, 2);
        }
    }
}
#[tokio::test]
async fn recording_failures_prevent_io_or_keep_the_delivered_outcome_uncertain() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_http BEFORE INSERT ON replay_events WHEN NEW.kind='slack_http_call' BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    assert!(matches!(
        h.web.send(post()).await,
        DeliveryOutcome::Ambiguous { .. }
    ));
    assert_eq!(h.server.calls.lock().unwrap().len(), 2);
    h.store.call(|c|{c.execute_batch("DROP TRIGGER fail_http; CREATE TRIGGER fail_result BEFORE INSERT ON replay_events WHEN NEW.kind='slack_http_result' BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    h.server.json(json!({"ok":true,"ts":"200.1"}));
    assert_eq!(
        h.web.send(post()).await,
        DeliveryOutcome::Ambiguous {
            code: "slack_recording_failed".into()
        }
    );
    assert_eq!(h.server.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn real_http_history_drives_durable_catchup_and_does_not_leak_method_arguments() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    h.server.json(json!({"ok":true,"messages":[{"ts":"999.1","user":"UALICE","text":"<@UOWNER> help"}],"response_metadata":{"next_cursor":"next & = cursor"}}));
    h.server.json(json!({"ok":true,"messages":[]}));
    let receiver = Receiver::new(
        h.store.clone(),
        h.web.config.clone(),
        h.clock.clone(),
        Arc::new(SequenceIds::default()),
    );
    let catchup = Catchup::new(receiver, h.web.clone(), Duration::from_secs(2)).unwrap();
    assert_eq!(catchup.run(900., None).await.unwrap().added, 1);
    {
        let calls = h.server.calls.lock().unwrap();
        assert!(calls[2].headers.starts_with("GET "));
        assert!(calls[2].path.starts_with("/api/conversations.history?"));
        let first: std::collections::BTreeMap<_, _> = h
            .server
            .base
            .join(&calls[2].path)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(first["include_all_metadata"], "true");
        assert!(!first.contains_key("method"));
        let second: std::collections::BTreeMap<_, _> = h
            .server
            .base
            .join(&calls[3].path)
            .unwrap()
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(second["cursor"], "next & = cursor");
    }
    let obligations: i64 = h
        .store
        .call(|c| Ok(c.query_row("SELECT count(*) FROM obligations", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(obligations, 1);
}
#[tokio::test]
async fn real_delivery_respects_outbox_retry_order_and_never_resends_uncertain_posts() {
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    let id = outbox::enqueue(&h.store, post().post, 1000.).await.unwrap();
    let mut reply = Reply::json(json!({"ok":false}));
    reply.status = 429;
    reply.headers.push(("Retry-After".into(), "7".into()));
    h.server.add(reply);
    assert_eq!(h.dispatcher().drain(10).await.unwrap(), 0);
    assert_eq!(h.dispatcher().drain(10).await.unwrap(), 0);
    assert_eq!(h.server.calls.lock().unwrap().len(), 3);
    h.clock.set(1008.);
    let mut reply = Reply::json(json!({}));
    reply.disconnect = true;
    h.server.add(reply);
    assert_eq!(h.dispatcher().drain(10).await.unwrap(), 0);
    outbox::recover(&h.store, 1009.).await.unwrap();
    h.clock.set(9999.);
    assert_eq!(h.dispatcher().drain(10).await.unwrap(), 0);
    assert_eq!(h.server.calls.lock().unwrap().len(), 4);
    let state: String = h
        .store
        .call(move |c| Ok(c.query_row("SELECT state FROM outbox WHERE id=?", [id], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(state, "ambiguous");
}

#[tokio::test]
async fn frozen_python_message_arguments_and_delivery_outcomes_match() {
    let corpus: Value = serde_json::from_str(include_str!("../corpus/slack.json")).unwrap();
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    for case in corpus["web"].as_array().unwrap() {
        let mut reply = Reply::json(case["body"].clone());
        reply.status = case["status"].as_u64().unwrap() as u16;
        for (key, value) in case["headers"].as_object().unwrap() {
            reply
                .headers
                .push((key.clone(), value.as_str().unwrap().into()));
        }
        h.server.add(reply);
        let mut actual = serde_json::to_value(h.web.send(post()).await).unwrap();
        actual.as_object_mut().unwrap().remove("code");
        assert_eq!(actual, case["expected"]);
        assert_eq!(
            serde_json::from_slice::<Value>(&h.server.calls.lock().unwrap().last().unwrap().body)
                .unwrap(),
            case["request"]
        );
    }
}
#[tokio::test]
async fn cancelling_a_send_leaves_durable_intent_and_recovery_never_resends() {
    let h = Harness::new(Duration::from_secs(10)).await;
    h.ready().await;
    outbox::enqueue(&h.store, post().post, 1000.).await.unwrap();
    let mut reply = Reply::json(json!({}));
    reply.hang = true;
    h.server.add(reply);
    let dispatcher = h.dispatcher();
    let task = tokio::spawn(async move { dispatcher.drain(1).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if h.server.calls.lock().unwrap().len() == 3 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    outbox::recover(&h.store, 1001.).await.unwrap();
    assert_eq!(h.dispatcher().drain(1).await.unwrap(), 0);
    let incomplete: i64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM replay_events WHERE kind='slack_http_call' AND complete=0",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(incomplete, 1);
    assert_eq!(h.server.calls.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn credential_echoes_are_scrubbed_from_json_bytes_headers_and_scope_metadata() {
    let h = Harness::new(Duration::from_secs(2)).await;
    let mut auth = Reply::json(json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM"}));
    auth.headers.push((
        "x-oauth-scopes".into(),
        "files:read,xoxp-private-test-secret".into(),
    ));
    h.server.add(auth);
    h.server
        .json(json!({"ok":true,"channel":{"id":"CROOM","is_member":true}}));
    h.web.validate().await.unwrap();
    let mut echo = Reply::json(
        json!({"ok":false,"error":"xoxp-private-test-secret","nested":{"token":"extra-secret"}}),
    );
    echo.headers
        .push(("Retry-After".into(), "xoxp-private-test-secret".into()));
    h.server.add(echo);
    let mut claim = post();
    claim.post.text = "xoxp-private-test-secret".into();
    h.web.send(claim).await;
    let mut raw = Reply::json(json!({}));
    raw.body = b"bad response xoxp-private-test-secret".to_vec();
    h.server.add(raw);
    h.web.send(post()).await;
    let ledger = h.ledger().await;
    assert!(!ledger.contains("xoxp-private-test-secret"));
    assert!(!ledger.contains("extra-secret"));
    // Binary boundary records use integer arrays, so inspect their decoded bytes.
    let records: Vec<String> = h
        .store
        .call(|c| {
            Ok(
                c.prepare("SELECT payload_json FROM replay_events WHERE kind='slack_http_result'")?
                    .query_map([], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?,
            )
        })
        .await
        .unwrap();
    for record in records {
        let value: Value = serde_json::from_str(&record).unwrap();
        if let Ok(bytes) = serde_json::from_value::<Vec<u8>>(value["body"]["bytes"].clone()) {
            assert!(!String::from_utf8_lossy(&bytes).contains("private-test-secret"));
        }
    }
    let scopes: String = h
        .store
        .call(|c| {
            Ok(
                c.query_row("SELECT value FROM meta WHERE key='slack_scopes'", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(!scopes.contains("private-test-secret"));
}

#[tokio::test]
async fn streaming_response_bounds_apply_without_content_length_and_reject_partial_bodies() {
    for (chunked, truncated, size) in [(true, false, ENVELOPE_LIMIT + 1), (false, true, 8)] {
        let h = Harness::new(Duration::from_secs(2)).await;
        h.ready().await;
        let mut reply = Reply::json(json!({}));
        reply.chunked = chunked;
        reply.truncated = truncated;
        reply.body = vec![b'x'; size];
        h.server.add(reply);
        assert!(matches!(
            h.web.send(post()).await,
            DeliveryOutcome::Ambiguous { .. }
        ));
        assert_eq!(h.server.calls.lock().unwrap().len(), 3);
        let incomplete: i64 = h
            .store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT count(*) FROM replay_events WHERE complete=0",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(incomplete, 2);
    }
}
