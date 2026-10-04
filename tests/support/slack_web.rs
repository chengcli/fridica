use super::*;
use crate::slack::ingress::ENVELOPE_LIMIT;
use crate::store::Store;
use crate::{
    config::{loader, LoadContext},
    core::{
        delivery::Post,
        time::{ReplayClock, SequenceIds},
    },
    slack::{catchup::Catchup, receiver::Receiver},
    store::outbox,
    threads::dispatcher::Dispatcher,
};
use reqwest::Url;
use sha2::Digest;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::Mutex,
    time::Duration,
};
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
        let mut auth = Reply::json(
            json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/"}),
        );
        auth.headers.push((
            "x-oauth-scopes".into(),
            "chat:write, files:read, files:write".into(),
        ));
        self.add(auth);
        self.json(json!({"ok":true,"channel":{"id":"CROOM","created":1,"is_member":true}}));
    }
}
struct Harness {
    _dir: tempfile::TempDir,
    server: Server,
    web: Arc<SlackClient>,
    config: Arc<crate::config::Config>,
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
        let web = client(
            &config,
            Arc::new(store.clone()),
            clock.clone(),
            "xoxp-private-test-secret".into(),
            timeout,
        )
        .unwrap()
        .with_test_endpoints(
            server.base.clone(),
            server.base.clone(),
            server.base.clone(),
        );
        Self {
            _dir: dir,
            server,
            web: Arc::new(SlackClient::from(web)),
            config,
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
    fn dispatcher(&self) -> Dispatcher<SlackClient> {
        Dispatcher {
            store: Arc::new(self.store.clone()),
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
            json!({"ok":true,"user_id":"UOWNER","team_id":"OTHER","team":"Team","url":"https://team.slack.com/"}),
            true,
        ),
        (
            json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/","bot_id":"B1"}),
            true,
        ),
        (
            json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/"}),
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
        // Slack reads upload arguments only from form fields, not a JSON body.
        let form = |index: usize| -> std::collections::BTreeMap<String, String> {
            assert!(calls[index]
                .headers
                .contains("application/x-www-form-urlencoded"));
            let body = String::from_utf8(calls[index].body.clone()).unwrap();
            Url::parse(&format!("http://form/?{body}"))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect()
        };
        assert_eq!(calls[2].path, "/api/files.getUploadURLExternal");
        assert_eq!(
            form(2),
            [("filename", "report.md"), ("length", "5")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into()
        );
        assert!(!calls[3].headers.contains("authorization"));
        assert_eq!(calls[3].body, b"hello");
        assert_eq!(calls[4].path, "/api/files.completeUploadExternal");
        let fields = form(4);
        assert_eq!(fields["channel_id"], "CROOM");
        assert_eq!(fields["thread_ts"], "100.1");
        let files: Value = serde_json::from_str(&fields["files"]).unwrap();
        assert_eq!(files, json!([{"id":"F123","title":"report.md"}]));
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
        // An explicit :443 is the same origin; the typed URL normalizes it away.
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
    assert_eq!(h.server.calls.lock().unwrap().len(), 7);
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
        Arc::new(h.store.clone()),
        h.config.clone(),
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
    let mut auth = Reply::json(
        json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/"}),
    );
    auth.headers.push((
        "x-oauth-scopes".into(),
        "files:read,xoxp-private-test-secret".into(),
    ));
    h.server.add(auth);
    h.server
        .json(json!({"ok":true,"channel":{"id":"CROOM","created":1,"is_member":true}}));
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
async fn startup_records_channel_names_for_owner_controls() {
    for (name, expected) in [
        (json!("ai-human-plume"), json!({"CROOM":"ai-human-plume"})),
        (json!("x".repeat(81)), json!({})),
        (Value::Null, json!({})),
    ] {
        let h = Harness::new(Duration::from_secs(2)).await;
        h.server
            .json(json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/"}));
        h.server.json(
            json!({"ok":true,"channel":{"id":"CROOM","created":1,"is_member":true,"name":name}}),
        );
        h.web.validate().await.unwrap();
        let names: String = h
            .store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT value FROM meta WHERE key='slack_channel_names'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&names).unwrap(), expected);
    }
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

fn file_reply(data: Vec<u8>, mime: &str) -> Reply {
    let mut reply = Reply::json(Value::Null);
    reply.body = data;
    reply.headers.push(("content-type".into(), mime.into()));
    reply
}
#[tokio::test]
async fn file_reads_require_validation_scope_and_a_trusted_origin() {
    use crate::slack::files::{Downloader, Failure as FileFailure};
    let h = Harness::new(Duration::from_secs(2)).await;
    let url = h.server.base.join("file").unwrap().to_string();
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::NotValidated)
    );
    h.ready().await;
    for bad in [
        "http://files.slack.com/a",
        "https://files.slack.com:443/a",
        "https://user@files.slack.com/a",
        "https://files.slack.com.evil/a",
        "https://files.slack.com/a#frag",
        "https://files.slack.com\\@evil/a",
        "https://files.slack.com/\na",
    ] {
        assert_eq!(
            h.web.download(bad.into(), false).await,
            Err(FileFailure::Url),
            "{bad}"
        );
    }
    h.web.set_file_scopes(Some(BTreeSet::new()));
    assert_eq!(
        h.web.download(url, false).await,
        Err(FileFailure::MissingScope)
    );
    assert_eq!(h.server.calls.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn file_reads_cap_streamed_bytes_and_record_cache_hits_without_exposing_tokens() {
    use crate::slack::files::{Downloader, FILE_LIMIT};
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    for chunked in [false, true] {
        let url = h
            .server
            .base
            .join(if chunked { "chunked" } else { "length" })
            .unwrap()
            .to_string();
        let mut reply = file_reply(vec![b'a'; FILE_LIMIT * 3], "text/plain");
        reply.chunked = chunked;
        h.server.add(reply);
        let first = h.web.download(url.clone(), false).await.unwrap();
        assert_eq!(first.data.len(), FILE_LIMIT + 1);
        assert_eq!(
            first.size,
            if chunked { 0 } else { (FILE_LIMIT * 3) as u64 }
        );
        assert_eq!(h.web.download(url, false).await.unwrap(), first);
    }
    let url = h.server.base.join("secret").unwrap().to_string();
    h.server.add(file_reply(
        b"echo xoxp-private-test-secret".to_vec(),
        "text/plain",
    ));
    assert_eq!(
        h.web.download(url, false).await.unwrap().data,
        b"echo [credential]"
    );
    let ledger = h.ledger().await;
    assert!(!ledger.contains("xoxp-private-test-secret"));
    assert!(ledger.contains("\"cache_hit\":true"));
    let calls = h.server.calls.lock().unwrap();
    assert_eq!(calls.len(), 5);
    assert!(calls[2..].iter().all(|r| r
        .headers
        .to_lowercase()
        .contains("authorization: bearer xoxp-private-test-secret")));
}
#[tokio::test]
async fn file_html_eligibility_is_checked_even_after_a_cached_read_and_redirects_are_not_followed()
{
    use crate::slack::files::{Downloader, Failure as FileFailure};
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    let url = h.server.base.join("html").unwrap().to_string();
    h.server.add(file_reply(
        b"<p>report</p>".to_vec(),
        "text/html; charset=utf-8",
    ));
    assert!(h.web.download(url.clone(), true).await.is_ok());
    h.server
        .add(file_reply(b"<p>report</p>".to_vec(), "text/html"));
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::Unavailable)
    );
    // Unknown scope must not use a successful HTML cache entry from a previous
    // validation. The scope state is part of the cache key.
    let unknown = url.clone();
    h.web.set_file_scopes(None);
    h.server
        .add(file_reply(b"<html>sign in</html>".to_vec(), "text/html"));
    assert_eq!(
        h.web.download(unknown, true).await,
        Err(FileFailure::UnknownHtml)
    );
    let url = h.server.base.join("redirect").unwrap().to_string();
    let mut redirect = file_reply(vec![], "text/plain");
    redirect.status = 302;
    redirect
        .headers
        .push(("location".into(), "https://evil.example/steal".into()));
    h.server.add(redirect);
    assert_eq!(
        h.web.download(url, false).await,
        Err(FileFailure::Unavailable)
    );
    assert_eq!(h.server.calls.lock().unwrap().len(), 6);
}
#[tokio::test]
async fn file_failures_expire_and_rate_limits_are_not_retried_early() {
    use crate::slack::files::{Downloader, Failure as FileFailure};
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    let url = h.server.base.join("file").unwrap().to_string();
    let mut rejected = file_reply(vec![], "text/plain");
    rejected.status = 403;
    h.server.add(rejected);
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::Unavailable)
    );
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::Unavailable)
    );
    h.clock.set(1301.);
    let mut limited = file_reply(vec![], "text/plain");
    limited.status = 429;
    limited.headers.push(("retry-after".into(), "7200".into()));
    h.server.add(limited);
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::RateLimited { retry_after: 7200. })
    );
    h.clock.set(2000.);
    assert_eq!(
        h.web.download(url.clone(), false).await,
        Err(FileFailure::RateLimited { retry_after: 7200. })
    );
    assert_eq!(h.server.calls.lock().unwrap().len(), 4);
    h.clock.set(8502.);
    h.server.add(file_reply(b"ok".to_vec(), "text/plain"));
    assert_eq!(h.web.download(url, false).await.unwrap().data, b"ok");
}
#[tokio::test]
async fn file_recording_faults_prevent_io_or_keep_the_snapshot_unfinished() {
    use crate::slack::files::{Downloader, Failure as FileFailure};
    for after in [false, true] {
        let h = Harness::new(Duration::from_secs(2)).await;
        h.ready().await;
        let kind = if after {
            "slack_file_result"
        } else {
            "slack_file_call"
        };
        h.store.call(move|c|{c.execute_batch(&format!("CREATE TRIGGER fail_file BEFORE INSERT ON replay_events WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'private SQL error'); END;"))?;Ok(())}).await.unwrap();
        if after {
            h.server.add(file_reply(b"body".to_vec(), "text/plain"));
        }
        let url = h.server.base.join("file").unwrap().to_string();
        assert_eq!(
            h.web.download(url, false).await,
            Err(FileFailure::Recording)
        );
        assert_eq!(
            h.server.calls.lock().unwrap().len(),
            if after { 3 } else { 2 }
        );
        if after {
            assert_eq!(h.store.call(|c|Ok(c.query_row("SELECT count(*) FROM replay_events WHERE kind='slack_file_call' AND complete=0",[],|r|r.get::<_,i64>(0))?)).await.unwrap(),1);
        }
    }
}
#[tokio::test]
async fn file_timeouts_partial_bodies_and_cancellation_never_claim_complete_bytes() {
    use crate::slack::files::{Downloader, Failure as FileFailure};
    for mode in ["timeout", "partial", "cancel"] {
        let h = Harness::new(Duration::from_millis(200)).await;
        h.ready().await;
        let mut reply = file_reply(b"partial".to_vec(), "text/plain");
        reply.hang = mode != "partial";
        reply.truncated = mode == "partial";
        h.server.add(reply);
        let url = h.server.base.join("file").unwrap().to_string();
        let web = h.web.clone();
        let task = tokio::spawn(async move { web.download(url, false).await });
        if mode == "cancel" {
            tokio::time::timeout(Duration::from_secs(2), async {
                while h.server.calls.lock().unwrap().len() < 3 {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(
                task.await.unwrap(),
                Err(if mode == "timeout" {
                    FileFailure::Timeout
                } else {
                    FileFailure::Connection
                })
            );
        }
        assert_eq!(h.store.call(|c|Ok(c.query_row("SELECT count(*) FROM replay_events WHERE kind='slack_file_call' AND complete=0",[],|r|r.get::<_,i64>(0))?)).await.unwrap(),1);
    }
}

fn linked_target() -> crate::slack::links::Link {
    crate::slack::links::Link {
        // Deliberately hostile: this label must never become an HTTP destination.
        link: "https://evil.invalid/steal".into(),
        channel: "CROOM".into(),
        ts: "201.000001".into(),
        root: Some("200.000001".into()),
    }
}
#[tokio::test]
async fn linked_fetch_uses_fixed_routes_scope_and_bounded_selection() {
    use crate::slack::links::{Failure as LinkFailure, Reader};
    let h = Harness::new(Duration::from_secs(2)).await;
    assert_eq!(
        h.web.fetch(linked_target()).await,
        Err(LinkFailure::Unavailable)
    );
    assert!(h.server.calls.lock().unwrap().is_empty());
    h.ready().await;
    let mut forbidden = linked_target();
    forbidden.channel = "COTHER".into();
    assert_eq!(h.web.fetch(forbidden).await, Err(LinkFailure::Unavailable));
    let mut invalid = linked_target();
    invalid.root = Some("200.1&channel=COTHER".into());
    assert_eq!(h.web.fetch(invalid).await, Err(LinkFailure::Unavailable));
    assert_eq!(h.server.calls.lock().unwrap().len(), 2);
    let corpus: Value = serde_json::from_str(include_str!("../corpus/links.json")).unwrap();
    for case in corpus["fetch"].as_array().unwrap() {
        let mut link = linked_target();
        link.ts = case["ts"].as_str().unwrap().into();
        link.root = case["root"].as_str().map(str::to_owned);
        h.server.json(json!({"ok":true,"messages":case["messages"],"response_metadata":{"next_cursor":"do-not-follow"}}));
        let output = json!(h.web.fetch(link).await.unwrap());
        let hash = format!(
            "{:x}",
            sha2::Sha256::digest(serde_json::to_vec(&output).unwrap())
        );
        assert_eq!(
            hash,
            case.get("rust_sha256")
                .unwrap_or(&case["sha256"])
                .as_str()
                .unwrap()
        );
        let calls = h.server.calls.lock().unwrap();
        let request = calls.last().unwrap();
        let url = h.server.base.join(&request.path).unwrap();
        assert_eq!(url.path(), "/api/conversations.replies");
        let params: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(params.len(), 3);
        assert_eq!(params["channel"], "CROOM");
        assert_eq!(params["limit"], "51");
        assert_eq!(params["ts"], case["calls"][0]["ts"].as_str().unwrap());
        assert!(request.body.is_empty());
    }
    let ledger = h.ledger().await;
    assert!(ledger.contains("linked_message"));
    assert!(!ledger.contains("xoxp-private-test-secret"));
    assert!(!ledger.contains("evil.invalid"));
}
#[tokio::test]
async fn linked_fetch_failures_are_bounded_visible_and_never_retried() {
    use crate::slack::links::{Failure as LinkFailure, Reader};
    let h = Harness::new(Duration::from_millis(50)).await;
    h.ready().await;
    let mut rate = Reply::json(json!({"ok":false,"error":"ratelimited"}));
    rate.status = 429;
    rate.headers.push(("retry-after".into(), "60".into()));
    let mut hanging = Reply::json(json!({}));
    hanging.hang = true;
    let mut partial = Reply::json(json!({"ok":true,"messages":[]}));
    partial.truncated = true;
    for reply in [
        Reply::json(json!({"ok":false,"error":"channel_not_found"})),
        Reply::json(json!({"ok":true,"messages":3})),
        rate,
        partial,
        hanging,
    ] {
        let before = h.server.calls.lock().unwrap().len();
        h.server.add(reply);
        assert_eq!(
            h.web.fetch(linked_target()).await,
            Err(LinkFailure::Unavailable)
        );
        assert_eq!(h.server.calls.lock().unwrap().len(), before + 1);
    }
}
#[tokio::test]
async fn linked_recording_faults_fail_closed_before_or_after_http() {
    use crate::slack::links::{Failure as LinkFailure, Reader};
    for after in [false, true] {
        let h = Harness::new(Duration::from_secs(2)).await;
        h.ready().await;
        let kind = if after {
            "slack_http_result"
        } else {
            "slack_http_call"
        };
        h.store.call(move |c| { c.execute_batch(&format!("CREATE TRIGGER fail_links BEFORE INSERT ON replay_events WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'private SQL error'); END;"))?; Ok(()) }).await.unwrap();
        if after {
            h.server.json(json!({"ok":true,"messages":[]}));
        }
        assert_eq!(
            h.web.fetch(linked_target()).await,
            Err(LinkFailure::Recording)
        );
        assert_eq!(h.server.calls.lock().unwrap().len(), 2 + usize::from(after));
        assert_eq!(h.store.call(|c| Ok(c.query_row("SELECT count(*) FROM replay_events WHERE kind='slack_http_call' AND complete=0", [], |r| r.get::<_, i64>(0))?)).await.unwrap(), i64::from(after));
    }
}

#[tokio::test]
async fn onboarding_discovery_uses_only_fixed_read_endpoints_without_a_database() {
    use crate::slack::discovery;
    let server = Server::new().await;
    server.json(json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","team":"Team","url":"https://team.slack.com/"}));
    server.json(json!({"ok":true,"channels":[{"id":"CROOM","created":1,"name":"general","is_member":true}],"response_metadata":{"next_cursor":"next +/?&="}}));
    server.json(json!({"ok":true,"channels":[]}));
    server.json(json!({"ok":false,"error":"missing_scope"}));
    let web = fridica_slack::discovery::Web::with_test_endpoint(
        "xoxp-onboarding-secret",
        server.base.clone(),
    )
    .unwrap();
    let found = discovery::discover(&web).await.unwrap();
    assert_eq!(found.owner, "UOWNER");
    assert_eq!(found.channels[0].id, "CROOM");
    assert!(found.warnings[0].contains("groups:read"));
    let calls = server.calls.lock().unwrap();
    assert_eq!(calls.len(), 4);
    assert_eq!(calls[0].path, "/api/auth.test");
    assert!(calls[0].headers.starts_with("GET "));
    for (index, private, cursor) in [(1, false, ""), (2, false, "next +/?&="), (3, true, "")] {
        let url = Url::parse(&format!("http://fixture{}", calls[index].path)).unwrap();
        assert_eq!(url.path(), "/api/conversations.list");
        assert!(calls[index].headers.starts_with("GET "));
        assert!(calls[index].body.is_empty());
        let args: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        // The first page of each type sends no cursor.
        let mut expected = json!({"types":if private {"private_channel"} else {"public_channel"},"exclude_archived":"true","limit":"200"});
        if !cursor.is_empty() {
            expected["cursor"] = json!(cursor);
        }
        assert_eq!(serde_json::to_value(args).unwrap(), expected);
        assert!(calls[index]
            .headers
            .to_ascii_lowercase()
            .contains("authorization: bearer xoxp-onboarding-secret"));
        assert!(!String::from_utf8_lossy(&calls[index].body).contains("secret"));
    }
}

#[tokio::test]
async fn onboarding_discovery_redacts_echoes_and_never_follows_redirects_or_retries() {
    use crate::slack::discovery::{Api, Request};
    let server = Server::new().await;
    let web = fridica_slack::discovery::Web::with_test_endpoint(
        "xoxp-onboarding-secret",
        server.base.clone(),
    )
    .unwrap();
    server.json(json!({"ok":true,"user_id":"UOWNER","team_id":"TTEAM","url":"https://team.slack.com/",
        "team":"echo xoxp-onboarding-secret","user":"xoxp-onboarding-secret","token":"xoxp-onboarding-secret"}));
    let value = web.get(Request::Identity).await.unwrap();
    assert!(!value.to_string().contains("xoxp-onboarding-secret"));
    let mut redirect = Reply::json(json!({"private":"xoxp-onboarding-secret"}));
    redirect.status = 302;
    redirect.headers.push((
        "location".into(),
        "https://example.invalid/xoxp-onboarding-secret".into(),
    ));
    server.add(redirect);
    assert_eq!(
        web.get(Request::Identity).await.unwrap_err(),
        Failure::InvalidResponse
    );
    server.json(json!({"ok":false,"error":"xoxp-onboarding-secret"}));
    let error = web.get(Request::Identity).await.unwrap_err();
    assert_eq!(
        error,
        Failure::Rejected {
            code: "slack_api_error".into()
        }
    );
    let mut rate = Reply::json(json!({"ok":false,"error":"ratelimited"}));
    rate.status = 429;
    rate.headers.push(("retry-after".into(), "2".into()));
    server.add(rate);
    assert_eq!(
        web.get(Request::Identity).await.unwrap_err(),
        Failure::RateLimited { retry_after: 2. }
    );
    let mut large = Reply::json(json!({}));
    large.body = vec![b'x'; ENVELOPE_LIMIT + 1];
    server.add(large);
    assert_eq!(
        web.get(Request::Identity).await.unwrap_err(),
        Failure::ResponseLimit
    );
    assert_eq!(server.calls.lock().unwrap().len(), 5);
    for bad in [
        "xoxb-private",
        "xapp-private",
        "xoxp-",
        "xoxp-private\nsecret",
    ] {
        let error = crate::slack::discovery::Web::new(bad)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains(bad));
    }
}
#[tokio::test]
async fn the_client_resolves_and_saves_a_file_to_disk() {
    use fridica_slack::files::Downloader;
    let h = Harness::new(Duration::from_secs(2)).await;
    h.ready().await;
    // files.info names the real host; the bytes come from the loopback server.
    let private = "https://files.slack.com/files-pri/TTEAM-F1/data.nc";
    h.server.json(
        json!({"ok":true,"file":{"id":"F1","created":1,"timestamp":1,"name":"data.nc",
        "mimetype":"application/octet-stream","url_private":private}}),
    );
    let mut data = Reply::json(json!(null));
    data.body = b"CDF\x01netcdf bytes".to_vec();
    h.server.add(data);
    assert_eq!(h.web.resolve("F1".into()).await.unwrap(), private);
    let url = h.server.base.join("/files-pri/TTEAM-F1/data.nc").unwrap();
    let target = h._dir.path().join("saved.nc");
    assert_eq!(
        h.web
            .save(url.to_string(), target.clone(), 1 << 20)
            .await
            .unwrap(),
        16
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"CDF\x01netcdf bytes");
    let ledger = h.ledger().await;
    assert!(ledger.contains("\"save\":true"), "{ledger}");
    assert!(!ledger.contains("xoxp-private-test-secret"));
}
