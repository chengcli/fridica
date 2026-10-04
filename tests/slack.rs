use fridica::{
    attention,
    config::{loader, Config, LoadContext},
    core::delivery::AdapterFuture,
    core::time::{ReplayClock, SequenceIds},
    slack::{
        catchup::{Catchup, History, HistoryFailure, PageRequest},
        ingress,
        receiver::Receiver,
    },
    store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

fn payload(ts: &str, text: &str) -> Value {
    json!({"type":"event_callback","event_id":format!("e{ts}"),"team_id":"TTEAM",
        "event":{"type":"message","channel":"CROOM","user":"UALICE","text":text,"ts":ts}})
}
fn envelope(payload: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"type":"events_api","envelope_id":"env1","payload":payload}))
        .unwrap()
}
struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    receiver: Receiver,
    clock: Arc<ReplayClock>,
}
impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
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
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let config: Arc<Config> = Arc::new(
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
        let clock = Arc::new(ReplayClock::new(2_000_000_000.));
        let receiver = Receiver::new(
            Arc::new(store.clone()),
            config,
            clock.clone(),
            Arc::new(SequenceIds::default()),
        );
        Self {
            _dir: dir,
            store,
            receiver,
            clock,
        }
    }
    async fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        self.store
            .call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
}
fn corpus() -> Value {
    serde_json::from_str(include_str!("corpus/slack.json")).unwrap()
}
#[test]
fn frozen_normalization_preserves_metadata_attachments_and_declared_hardening() {
    for case in corpus()["normalization"].as_array().unwrap() {
        let normalized = ingress::normalize(&case["payload"], "socket");
        let mut actual = serde_json::to_value(normalized).unwrap();
        if actual.is_object() && actual.get("files").is_none() {
            actual["files"] = json!([]);
        }
        assert_eq!(actual, case["expected"], "payload {}", case["payload"]);
        if !case["exception"].is_null() {
            assert_ne!(case["python"], case["expected"]);
        } else {
            assert_eq!(case["python"], case["expected"]);
        }
    }
}
#[tokio::test]
async fn socket_ack_requires_durable_atomic_intake_and_redelivery_deduplicates() {
    let h = Harness::new().await;
    let mut event = payload("100.1", "<@UOWNER> help");
    event["token"] = json!("verification-secret");
    let input = envelope(event);
    h.store.call(|c| {c.execute_batch("CREATE TRIGGER fail_obligation BEFORE INSERT ON obligations BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    assert!(h.receiver.receive(&input).await.is_err());
    for table in ["messages", "thread_inbox", "obligations", "replay_events"] {
        assert_eq!(h.count(table).await, 0);
    }
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_obligation")?;
            Ok(())
        })
        .await
        .unwrap();
    let ack = h.receiver.receive(&input).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(ack).unwrap(),
        json!({"envelope_id":"env1"})
    );
    assert_eq!(h.count("obligations").await, 1);
    // Lost ack: Slack retries the event with the same or a different envelope ID.
    h.receiver.receive(&input).await.unwrap();
    let mut duplicate: Value = serde_json::from_slice(&input).unwrap();
    duplicate["envelope_id"] = json!("env2");
    duplicate["payload"]["event_id"] = json!("new-event-id");
    h.receiver
        .receive(&serde_json::to_vec(&duplicate).unwrap())
        .await
        .unwrap();
    for table in ["messages", "thread_inbox", "obligations"] {
        assert_eq!(h.count(table).await, 1);
    }
    assert_eq!(h.count("replay_events").await, 6);
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
    assert!(!raw.contains("verification-secret"));
}
#[tokio::test]
async fn dropped_mentions_are_visible_and_untrusted_envelopes_cannot_control_threads() {
    let h = Harness::new().await;
    let mut dropped = payload("100.1", "<@UOWNER> help");
    dropped["event"]["subtype"] = json!("message_changed");
    h.receiver.receive(&envelope(dropped)).await.unwrap();
    assert_eq!(h.count("health_events").await, 1);
    for change in [("team_id", "TOTHER"), ("channel", "COTHER")] {
        let mut input = payload("100.2", "<@UOWNER> help");
        if change.0 == "team_id" {
            input["team_id"] = json!(change.1);
        } else {
            input["event"]["channel"] = json!(change.1);
        }
        h.receiver.receive(&envelope(input)).await.unwrap();
    }
    h.receiver.receive(br#"{"type":"interactive","envelope_id":"x","payload":{"token":"secret","command":"resume"}}"#).await.unwrap();
    assert_eq!(h.count("messages").await, 0);
    assert_eq!(h.count("obligations").await, 0);
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
    assert!(!raw.contains("secret"));
    assert_eq!(
        h.receiver.receive(br#"{"type":"hello"}"#).await.unwrap(),
        None
    );
    assert_eq!(
        h.receiver
            .receive(br#"{"type":"disconnect"}"#)
            .await
            .unwrap(),
        None
    );
    for bytes in [
        b"invalid".to_vec(),
        b"[]".to_vec(),
        b"null".to_vec(),
        b"42".to_vec(),
        br#"{"type":"events_api"}"#.to_vec(),
        vec![b'x'; ingress::ENVELOPE_LIMIT + 1],
    ] {
        assert!(h.receiver.receive(&bytes).await.is_err());
    }
}
#[tokio::test]
async fn owner_pause_and_filename_only_shares_remain_durable_at_intake() {
    let h = Harness::new().await;
    h.receiver
        .receive(&envelope(payload("100.1", "start")))
        .await
        .unwrap();
    h.store
        .call(|c| {
            c.execute(
                "UPDATE threads SET control='paused',pause_reason='owner'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let mut mention = payload("100.2", "<@UOWNER> follow up");
    mention["event"]["thread_ts"] = json!("100.1");
    h.receiver.receive(&envelope(mention)).await.unwrap();
    let control: String = h
        .store
        .call(|c| Ok(c.query_row("SELECT control FROM threads", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(control, "paused");
    assert_eq!(h.count("obligations").await, 1);
    let mut shared = payload("100.3", "");
    shared["event"]["files"] = json!([{"name":"plot.png"}]);
    h.receiver.receive(&envelope(shared)).await.unwrap();
    let files: String = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT files_json FROM messages WHERE ts='100.3'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(files, r#"["plot.png"]"#);
}
#[derive(Default)]
struct FakeHistory {
    responses: Mutex<VecDeque<std::result::Result<Value, HistoryFailure>>>,
    calls: Mutex<Vec<PageRequest>>,
    hang: bool,
}
impl History for FakeHistory {
    fn page(
        &self,
        request: PageRequest,
    ) -> AdapterFuture<'_, std::result::Result<Value, HistoryFailure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(request);
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected history call")
        })
    }
}
async fn projection(store: &Store) -> Value {
    store.call(|c| {
        let mut result=json!({});
        for (key,sql) in [
            ("messages","SELECT json_object('event_id',event_id,'ts',ts,'thread_ts',thread_ts,'text',text,'files_json',files_json,'source',source,'meta_json',meta_json,'attachments_json',attachments_json) FROM messages ORDER BY id"),
            ("threads","SELECT json_object('id',id,'status',status,'created',created,'updated',updated) FROM threads ORDER BY id"),
            ("inbox","SELECT json_object('session_id',session_id,'kind',kind,'ref',ref,'state',state) FROM thread_inbox ORDER BY id")
        ] {
            let rows:Vec<String>=c.prepare(sql)?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            result[key]=json!(rows.iter().map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>());
        }
        for (field,key) in [("mark","catchup:TTEAM:CROOM"),("truncated","catchup:TTEAM:CROOM:truncated")] {
            result[field]=c.query_row("SELECT (SELECT value FROM meta WHERE key=?)",[key],|r|r.get::<_,Option<String>>(0))?.map_or(Value::Null,Value::String);
        }
        Ok(result)
    }).await.unwrap()
}
#[tokio::test]
async fn frozen_catchup_pages_watermarks_and_history_effects_match() {
    for case in corpus()["catchup"].as_array().unwrap() {
        let h = Harness::new().await;
        for (i, seed) in case["seeds"].as_array().into_iter().flatten().enumerate() {
            let ts = format!("{:.6}", seed["ts"].as_f64().unwrap());
            let mut input = payload(&ts, "earlier");
            input["event_id"] = json!(format!("seed{i}"));
            let message = ingress::normalize(&input, "socket").unwrap();
            attention::intake(
                &h.store,
                message,
                "UOWNER".into(),
                seed["received"]
                    .as_f64()
                    .unwrap_or(seed["ts"].as_f64().unwrap()),
                3600.,
                format!("seed-ob{i}"),
            )
            .await
            .unwrap();
            if seed["waiting"] == true {
                h.store
                    .call(|c| {
                        c.execute("UPDATE threads SET status='waiting'", [])?;
                        Ok(())
                    })
                    .await
                    .unwrap();
            }
        }
        let fake = Arc::new(FakeHistory::default());
        let service =
            Catchup::new(h.receiver.clone(), fake.clone(), Duration::from_secs(1)).unwrap();
        for run in case["runs"].as_array().unwrap() {
            h.clock.set(run["now"].as_f64().unwrap());
            *fake.responses.lock().unwrap() = run["responses"]
                .as_array()
                .unwrap()
                .iter()
                .cloned()
                .map(Ok)
                .collect();
            fake.calls.lock().unwrap().clear();
            let result = service
                .run(
                    run["window"].as_f64().unwrap_or(3600.),
                    run["started"].as_f64(),
                )
                .await
                .unwrap();
            assert_eq!(
                !result.incomplete_channels.is_empty(),
                run["incomplete"].as_bool().unwrap()
            );
            if let Some(added) = run["added"].as_u64() {
                assert_eq!(result.added, added as usize);
            }
            assert_eq!(
                projection(&h.store).await,
                run["expected"],
                "case {}",
                case["name"]
            );
            assert_eq!(
                json!(*fake.calls.lock().unwrap()),
                run["calls"],
                "case {}",
                case["name"]
            );
            assert!(fake.responses.lock().unwrap().is_empty());
        }
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
        assert_eq!(incomplete, 0);
        if case["name"] == "truncation" {
            assert_eq!(h.count("health_events").await, 1);
        }
        if case["name"] == "overlap" {
            assert_eq!(h.count("obligations").await, 1);
        }
    }
}
#[tokio::test]
async fn history_errors_timeout_and_bad_responses_never_advance_watermarks() {
    for response in [
        Err(HistoryFailure::RateLimited { retry_after: 7. }),
        Err(HistoryFailure::Connection),
        Ok(json!({"ok":false,"error":"internal_error"})),
        Ok(json!({"ok":true,"messages":null})),
        Ok(json!({"ok":true,"messages":[],"response_metadata":{"next_cursor":123}})),
    ] {
        let h = Harness::new().await;
        let fake = Arc::new(FakeHistory::default());
        fake.responses.lock().unwrap().push_back(response);
        let service = Catchup::new(h.receiver.clone(), fake, Duration::from_secs(1)).unwrap();
        assert!(service.run(3600., None).await.is_err());
        assert_eq!(h.count("channel_watermarks").await, 0);
        assert_eq!(h.count("messages").await, 0);
        assert_eq!(h.count("replay_events").await, 2);
    }
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory {
        hang: true,
        ..Default::default()
    });
    let service = Catchup::new(h.receiver.clone(), fake, Duration::from_millis(10)).unwrap();
    assert!(service.run(3600., None).await.is_err());
    assert_eq!(h.count("channel_watermarks").await, 0);
}
#[tokio::test]
async fn cancellation_and_recording_failure_leave_no_false_completion() {
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory {
        hang: true,
        ..Default::default()
    });
    let service =
        Arc::new(Catchup::new(h.receiver.clone(), fake.clone(), Duration::from_secs(30)).unwrap());
    let task = {
        let service = service.clone();
        tokio::spawn(async move { service.run(3600., None).await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if !fake.calls.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    assert_eq!(h.count("channel_watermarks").await, 0);
    let complete: i64 = h
        .store
        .call(|c| Ok(c.query_row("SELECT complete FROM replay_events", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(complete, 0);
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_record BEFORE INSERT ON replay_events BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    assert!(service.run(3600., None).await.is_err());
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn catchup_watermark_and_intake_commit_together() {
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory::default());
    let response = json!({"ok":true,"messages":[{"user":"UALICE","text":"<@UOWNER> help","ts":"1999999999.1"}]});
    fake.responses
        .lock()
        .unwrap()
        .push_back(Ok(response.clone()));
    let service = Catchup::new(h.receiver.clone(), fake.clone(), Duration::from_secs(1)).unwrap();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_mark BEFORE INSERT ON channel_watermarks BEGIN SELECT RAISE(ABORT,'test'); END")?;Ok(())}).await.unwrap();
    assert!(service.run(3600., None).await.is_err());
    for table in [
        "messages",
        "thread_inbox",
        "obligations",
        "channel_watermarks",
    ] {
        assert_eq!(h.count(table).await, 0);
    }
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_mark")?;
            Ok(())
        })
        .await
        .unwrap();
    fake.responses.lock().unwrap().push_back(Ok(response));
    assert_eq!(service.run(3600., None).await.unwrap().added, 1);
    assert_eq!(h.count("obligations").await, 1);
    let mark: f64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT last_complete_pass FROM channel_watermarks",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mark, 2_000_000_000.);
}

#[tokio::test]
async fn missing_cursor_cannot_certify_completion_and_oversize_is_not_exact_replay() {
    // Deliberate hardening beyond Python _pages: has_more without a cursor
    // indicates an unread range, even though Python calls this complete.
    assert_eq!(corpus()["paging_exception"]["python_complete"], true);
    assert_eq!(corpus()["paging_exception"]["rust_complete"], false);
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory::default());
    fake.responses
        .lock()
        .unwrap()
        .push_back(Ok(corpus()["paging_exception"]["response"].clone()));
    let service = Catchup::new(h.receiver.clone(), fake, Duration::from_secs(1)).unwrap();
    let progress = service.run(3600., None).await.unwrap();
    assert_eq!(progress.incomplete_channels, vec!["CROOM"]);
    let pinned: i64 = h
        .store
        .call(|c| Ok(c.query_row("SELECT pinned FROM channel_watermarks", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(pinned, 1);
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory::default());
    fake.responses.lock().unwrap().push_back(Ok(
        json!({"ok":true,"messages":[],"large":"x".repeat(ingress::ENVELOPE_LIMIT)}),
    ));
    let service = Catchup::new(h.receiver.clone(), fake, Duration::from_secs(1)).unwrap();
    assert!(service.run(3600., None).await.is_err());
    assert_eq!(h.count("channel_watermarks").await, 0);
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
struct HeldHistory {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl History for HeldHistory {
    fn page(
        &self,
        _: PageRequest,
    ) -> AdapterFuture<'_, std::result::Result<Value, HistoryFailure>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(
                json!({"ok":true,"messages":[{"ts":"1999999999.1","user":"UALICE","text":"<@UOWNER> help"}]}),
            )
        })
    }
}
#[tokio::test]
async fn concurrent_pass_cannot_overwrite_another_watermark() {
    let h = Harness::new().await;
    let held = Arc::new(HeldHistory {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let slow = Catchup::new(h.receiver.clone(), held.clone(), Duration::from_secs(10)).unwrap();
    let task = tokio::spawn(async move { slow.run(3600., None).await });
    tokio::time::timeout(Duration::from_secs(2), held.entered.notified())
        .await
        .unwrap();
    h.clock.set(2_000_000_100.);
    let fake = Arc::new(FakeHistory::default());
    fake.responses
        .lock()
        .unwrap()
        .push_back(Ok(json!({"ok":true,"messages":[]})));
    let fast = Catchup::new(h.receiver.clone(), fake, Duration::from_secs(1)).unwrap();
    fast.run(3600., None).await.unwrap();
    held.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert_eq!(h.count("messages").await, 0);
    let mark: f64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT last_complete_pass FROM channel_watermarks",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mark, 2_000_000_100.);
}

#[tokio::test]
async fn unsupported_caught_up_mentions_are_visible_without_repeating_health_events() {
    let h = Harness::new().await;
    let fake = Arc::new(FakeHistory::default());
    let response = json!({"ok":true,"messages":[{"ts":"1999999999.1","user":"UALICE","subtype":"bot_message","text":"<@UOWNER> help"}]});
    fake.responses
        .lock()
        .unwrap()
        .extend([Ok(response.clone()), Ok(response)]);
    let service = Catchup::new(h.receiver.clone(), fake, Duration::from_secs(1)).unwrap();
    assert_eq!(service.run(3600., None).await.unwrap().added, 0);
    assert_eq!(service.clone().run(900., None).await.unwrap().added, 0);
    assert_eq!(h.count("messages").await, 0);
    assert_eq!(h.count("health_events").await, 1);
}

#[tokio::test]
async fn a_deleted_thread_root_is_skipped_and_the_watermark_still_advances() {
    let h = Harness::new().await;
    h.clock.set(1000.);
    let fake = Arc::new(FakeHistory::default());
    let message = |ts: &str, text: &str| json!({"ts":ts,"user":"UALICE","text":text});
    let root = |ts: &str| json!({"ts":ts,"user":"UALICE","text":"root","reply_count":1});
    let replies = |root: &str, ts: &str| {
        Ok(
            json!({"ok":true,"messages":[message(root,"root"),{"ts":ts,"user":"UALICE","text":"reply","thread_ts":root}]}),
        )
    };
    let pass = || {
        vec![
            Ok(json!({"ok":true,"messages":[root("990.1"),root("995.1"),root("998.1")]})),
            replies("990.1", "991.1"),
            // Root 995.1 was deleted in Slack.
            Err(HistoryFailure::Rejected {
                code: "thread_not_found".into(),
            }),
            replies("998.1", "999.1"),
        ]
    };
    *fake.responses.lock().unwrap() = pass().into();
    let service = Catchup::new(h.receiver.clone(), fake.clone(), Duration::from_secs(1)).unwrap();
    let progress = service.run(3600., None).await.unwrap();
    assert!(progress.incomplete_channels.is_empty());
    // The other roots' replies are read; the watermark advances to this pass.
    let texts: Vec<String> = h
        .store
        .call(|c| {
            Ok(c.prepare("SELECT ts FROM messages ORDER BY ts")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(texts, ["990.1", "991.1", "995.1", "998.1", "999.1"]);
    let mark: f64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT last_complete_pass FROM channel_watermarks",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mark, 1000.);
    let skipped = || async {
        h.store
            .call(|c| {
                Ok(c.prepare("SELECT details_json FROM health_events WHERE kind='slack_catchup_skipped_thread'")?
                    .query_map([], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .await
            .unwrap()
    };
    let rows = skipped().await;
    assert_eq!(rows.len(), 1);
    let row: Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(
        (row["root"].as_str(), row["code"].as_str()),
        (Some("995.1"), Some("thread_not_found"))
    );
    // The next pass meets the same root: still complete, recorded once.
    h.clock.set(1300.);
    *fake.responses.lock().unwrap() = pass().into();
    assert!(service
        .run(3600., None)
        .await
        .unwrap()
        .incomplete_channels
        .is_empty());
    assert_eq!(skipped().await.len(), 1);
    // A refused channel history still fails the pass and leaves the watermark.
    h.clock.set(1600.);
    *fake.responses.lock().unwrap() = vec![Err(HistoryFailure::Rejected {
        code: "channel_not_found".into(),
    })]
    .into();
    let error = service.run(3600., None).await.unwrap_err().to_string();
    assert!(error.contains("channel_not_found"), "{error}");
    let mark: f64 = h
        .store
        .call(|c| {
            Ok(c.query_row(
                "SELECT last_complete_pass FROM channel_watermarks",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(mark, 1300.);
}
