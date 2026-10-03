use fridica::{
    core::{
        delivery::{AdapterFuture, ClaimedPost, Delivery, DeliveryOutcome, Post},
        time::ReplayClock,
        Authority,
    },
    store::{outbox, Store},
    threads::dispatcher::Dispatcher,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

fn post(key: &str, thread: &str, after: &str) -> Post {
    Post {
        idem_key: key.into(),
        session_id: format!("TTEAM:CROOM:{thread}"),
        kind: "reply".into(),
        channel: "CROOM".into(),
        thread_ts: Some(thread.into()),
        text: key.into(),
        meta: Some(json!({"owner":"UOWNER","kind":"reply"})),
        filename: String::new(),
        blob: None,
        after: after.into(),
    }
}
async fn state(s: &Store, id: i64) -> String {
    s.call(move |c| Ok(c.query_row("SELECT state FROM outbox WHERE id=?", [id], |r| r.get(0))?))
        .await
        .unwrap()
}
#[derive(Default)]
struct Fake {
    results: Mutex<VecDeque<DeliveryOutcome>>,
    calls: Mutex<Vec<String>>,
}
impl Delivery for Fake {
    fn send(&self, p: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(p.post.idem_key);
            self.results
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DeliveryOutcome::Sent {
                    reference: format!("200.{}", p.id),
                })
        })
    }
}
fn dispatcher(s: &Store, adapter: Arc<Fake>, clock: Arc<ReplayClock>) -> Dispatcher<Fake> {
    Dispatcher {
        store: s.clone(),
        delivery: adapter,
        clock,
        owner: "UOWNER".into(),
        observe_only: false,
        timeout: Duration::from_secs(1),
    }
}

#[tokio::test]
async fn delivery_matches_batch_order_and_records_self_history_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    outbox::enqueue(&s, post("a", "100.1", ""), 1.)
        .await
        .unwrap();
    outbox::enqueue(&s, post("a-child", "100.1", "a"), 1.)
        .await
        .unwrap();
    outbox::enqueue(&s, post("b", "100.2", ""), 1.)
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    let d = dispatcher(&s, fake.clone(), Arc::new(ReplayClock::new(2.)));
    assert_eq!(d.drain(100).await.unwrap(), 3);
    assert_eq!(*fake.calls.lock().unwrap(), vec!["a", "b", "a-child"]);
    let (count, metadata): (i64, String) = s
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*),meta_json FROM messages WHERE source='self'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(count, 3);
    assert_eq!(
        serde_json::from_str::<Value>(&metadata).unwrap()["owner"],
        "UOWNER"
    );
}

#[tokio::test]
async fn ambiguity_blocks_descendants_and_never_automatically_resends() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let id = outbox::enqueue(&s, post("a", "100.1", ""), 1.)
        .await
        .unwrap();
    let child = outbox::enqueue(&s, post("child", "100.1", "a"), 1.)
        .await
        .unwrap();
    let grand = outbox::enqueue(&s, post("grand", "100.1", "child"), 1.)
        .await
        .unwrap();
    let fake = Arc::new(Fake::default());
    fake.results
        .lock()
        .unwrap()
        .push_back(DeliveryOutcome::Ambiguous {
            code: "disconnected".into(),
        });
    let d = dispatcher(&s, fake.clone(), Arc::new(ReplayClock::new(2.)));
    assert_eq!(d.drain(100).await.unwrap(), 0);
    assert_eq!(d.drain(100).await.unwrap(), 0);
    assert_eq!(state(&s, id).await, "ambiguous");
    assert_eq!(state(&s, child).await, "blocked");
    assert_eq!(state(&s, grand).await, "blocked");
    let late = outbox::enqueue(&s, post("late", "100.1", "a"), 3.)
        .await
        .unwrap();
    assert_eq!(state(&s, late).await, "blocked");
    assert!(outbox::requeue(&s, id, Authority::System, 4.)
        .await
        .is_err());
    assert!(outbox::requeue(&s, id, Authority::Owner, 4.).await.unwrap());
    assert_eq!(d.drain(100).await.unwrap(), 4);
    assert_eq!(fake.calls.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn claims_are_atomic_and_old_completion_cannot_acknowledge_retry() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let id = outbox::enqueue(&s, post("a", "100.1", ""), 1.)
        .await
        .unwrap();
    let (a, b) = tokio::join!(outbox::claim(&s, 2.), outbox::claim(&s, 2.));
    let old = a.unwrap().or(b.unwrap()).unwrap();
    assert!(outbox::claim(&s, 2.).await.unwrap().is_none());
    assert_eq!(outbox::recover(&s, 3.).await.unwrap(), 1);
    assert_eq!(outbox::recover(&s, 3.).await.unwrap(), 0);
    outbox::requeue(&s, id, Authority::Owner, 4.).await.unwrap();
    let current = outbox::claim(&s, 4.).await.unwrap().unwrap();
    assert!(outbox::complete(
        &s,
        old,
        DeliveryOutcome::Sent {
            reference: "5.0".into()
        },
        "UOWNER".into(),
        5.
    )
    .await
    .is_err());
    assert_eq!(state(&s, id).await, "sending");
    outbox::complete(
        &s,
        current,
        DeliveryOutcome::Sent {
            reference: "6.0".into(),
        },
        "UOWNER".into(),
        6.,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn rate_limits_are_durable_bounded_and_manual_retry_gets_a_new_budget() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let id = outbox::enqueue(&s, post("a", "100.1", ""), 1.)
        .await
        .unwrap();
    for at in 1..=5 {
        let claimed = outbox::claim(&s, at as f64).await.unwrap().unwrap();
        outbox::complete(
            &s,
            claimed,
            DeliveryOutcome::RateLimited { retry_after: 0. },
            "UOWNER".into(),
            at as f64,
        )
        .await
        .unwrap();
        assert!(outbox::claim(&s, at as f64).await.unwrap().is_none());
    }
    assert_eq!(state(&s, id).await, "failed");
    outbox::requeue(&s, id, Authority::Owner, 10.)
        .await
        .unwrap();
    let attempt = outbox::claim(&s, 10.).await.unwrap().unwrap();
    assert_eq!(attempt.attempt, 6);
    outbox::complete(
        &s,
        attempt,
        DeliveryOutcome::RateLimited { retry_after: 50. },
        "UOWNER".into(),
        10.,
    )
    .await
    .unwrap();
    assert_eq!(state(&s, id).await, "pending");
    assert!(outbox::claim(&s, 59.).await.unwrap().is_none());
    assert!(outbox::claim(&s, 60.).await.unwrap().is_some());
}

#[tokio::test]
async fn upload_history_observe_only_and_idempotency_rules() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let mut upload = post("file", "100.1", "");
    upload.kind = "upload".into();
    upload.blob = Some(vec![1, 2, 3]);
    upload.filename = "result.bin".into();
    let id = outbox::enqueue(&s, upload.clone(), 1.).await.unwrap();
    assert_eq!(outbox::enqueue(&s, upload.clone(), 1.).await.unwrap(), id);
    upload.blob = Some(vec![4]);
    assert!(outbox::enqueue(&s, upload, 1.).await.is_err());
    let fake = Arc::new(Fake::default());
    let mut d = dispatcher(&s, fake.clone(), Arc::new(ReplayClock::new(2.)));
    d.observe_only = true;
    assert_eq!(d.drain(100).await.unwrap(), 0);
    assert!(fake.calls.lock().unwrap().is_empty());
    assert_eq!(state(&s, id).await, "pending");
    d.observe_only = false;
    assert_eq!(d.drain(100).await.unwrap(), 1);
    assert_eq!(
        s.call(|c| Ok(c.query_row("SELECT count(*) FROM messages", [], |r| r.get::<_, i64>(0))?))
            .await
            .unwrap(),
        0
    );
}

struct HangingDelivery;
impl Delivery for HangingDelivery {
    fn send(&self, _: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test]
async fn delivery_timeout_is_ambiguous_and_never_automatically_resent() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let id = outbox::enqueue(&s, post("a", "100.1", ""), 1.)
        .await
        .unwrap();
    let dispatcher = Dispatcher {
        store: s.clone(),
        delivery: Arc::new(HangingDelivery),
        clock: Arc::new(ReplayClock::new(2.)),
        owner: "UOWNER".into(),
        observe_only: false,
        timeout: Duration::from_millis(1),
    };
    assert_eq!(dispatcher.drain(10).await.unwrap(), 0);
    assert_eq!(state(&s, id).await, "ambiguous");
    assert_eq!(dispatcher.drain(10).await.unwrap(), 0);
    let (attempts, error, incomplete): (i64, String, i64) = s.call(|c| {
        Ok(c.query_row("SELECT attempts,error,(SELECT count(*) FROM replay_events WHERE complete=0) FROM outbox", [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?)
    }).await.unwrap();
    assert_eq!(
        (attempts, error.as_str(), incomplete),
        (1, "delivery_timeout", 0)
    );
}

#[tokio::test]
async fn the_egress_gate_holds_back_private_terms_and_ai_trailers_without_naming_them() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let deny = dir.path().join("deny.txt");
    std::fs::write(&deny, "# names that stay private\nJane Q\\. Private\n").unwrap();
    std::fs::set_permissions(&deny, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut texts = vec![];
    // One thread each: the outbox delivers a thread's posts in order.
    for (key, thread, text) in [
        ("clean", "100.1", "Checks passed."),
        (
            "private",
            "100.2",
            "Thanks to jane q. private for the data.",
        ),
        (
            "trailer",
            "100.3",
            "Done.\nCo-Authored-By: Claude <noreply@anthropic.com>",
        ),
        (
            "both",
            "100.5",
            "Thanks to Jane Q. Private.\nCo-Authored-By: Claude <noreply@anthropic.com>",
        ),
    ] {
        let mut p = post(key, thread, "");
        p.text = text.into();
        texts.push(outbox::enqueue(&s, p, 1.).await.unwrap());
    }
    let mut upload = post("upload", "100.4", "");
    upload.kind = "upload".into();
    upload.filename = "notes.md".into();
    upload.blob = Some(b"Reviewed with Jane Q. Private".to_vec());
    texts.push(outbox::enqueue(&s, upload, 1.).await.unwrap());
    let fake = Arc::new(Fake::default());
    let clock = Arc::new(ReplayClock::new(10.));
    let d = dispatcher(&s, fake.clone(), clock);
    assert_eq!(d.drain_checked(10, Some(&deny)).await.unwrap(), 1);
    assert_eq!(*fake.calls.lock().unwrap(), ["clean"]);
    let errors: Vec<(String, String)> = s
        .call(|c| {
            Ok(c.prepare("SELECT state,error FROM outbox ORDER BY id")?
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(
        errors[1..],
        [
            ("failed".into(), "egress_deny_list_2".into()),
            ("failed".into(), "egress_ai_trailer".into()),
            // Every broken rule, so one fix round is enough.
            (
                "failed".into(),
                "egress_ai_trailer+egress_deny_list_2".into()
            ),
            ("failed".into(), "egress_deny_list_2".into()),
        ]
    );
    // The refusals name the rule only: the matched term is in no delivery record.
    let records: String = s
        .call(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(group_concat(payload_json),'') FROM replay_events WHERE kind='delivery'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(!records.to_lowercase().contains("jane q"), "{records}");
    // An unreadable or public list holds every post back.
    let mut held = post("held", "200.1", "");
    held.text = "Fine text".into();
    outbox::enqueue(&s, held, 11.).await.unwrap();
    std::fs::set_permissions(&deny, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(d.drain_checked(10, Some(&deny)).await.unwrap(), 0);
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    let health: i64 = s
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM health_events WHERE kind='egress_deny_list_unavailable'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(health, 1);
}

/// Sign-off lines are the agent's responsibility (provision04, #109): they
/// are delivered like any other text, whatever repository the thread names,
/// across repositories, and beside prose that starts with "Sign-off".
#[tokio::test]
async fn sign_off_lines_are_delivered_without_a_head_check() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let head = "0123456789abcdef0123456789abcdef01234567";
    s.call(|c| {
        for (thread, context) in [
            ("100.1", r#"{"repo":"kintera-cli"}"#),
            ("100.2", "{}"),
            ("100.3", r#"{"repo":"owner/project"}"#),
        ] {
            c.execute(
                "INSERT INTO threads(id,workspace,channel,root_ts,created,updated,context_json)
                 VALUES(?,'TTEAM','CROOM',?,1,1,?)",
                rusqlite::params![format!("TTEAM:CROOM:{thread}"), thread, context],
            )?;
        }
        Ok(())
    })
    .await
    .unwrap();
    for (key, thread, text) in [
        ("registry", "100.1", format!("SIGN-OFF #138 {head} approve")),
        (
            "two_repos",
            "100.2",
            format!(
                "SIGN-OFF owner/one#7 {} approve\nSIGN-OFF owner/two#8 {head} changes",
                &head[..7]
            ),
        ),
        (
            "prose",
            "100.3",
            format!("SIGN-OFF #7 {head} approve\nSign-off for #7, posted alone."),
        ),
    ] {
        let mut p = post(key, thread, "");
        p.text = text;
        outbox::enqueue(&s, p, 1.).await.unwrap();
    }
    let fake = Arc::new(Fake::default());
    let d = dispatcher(&s, fake.clone(), Arc::new(ReplayClock::new(10.)));
    assert_eq!(d.drain_checked(10, None).await.unwrap(), 3);
    assert_eq!(
        *fake.calls.lock().unwrap(),
        ["registry", "two_repos", "prose"]
    );
}

#[tokio::test]
async fn a_refused_reply_queues_one_rewrite_turn_and_its_refusal_queues_none() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    s.call(|c| {
        c.execute(
            "INSERT INTO threads(id,workspace,channel,root_ts,created,updated,control) VALUES('TTEAM:CROOM:100.1','TTEAM','CROOM','100.1',1,1,'active')",
            [],
        )?;
        // The inbox items the posts answer: a message, then the rewrite turn.
        c.execute("INSERT INTO thread_inbox(id,session_id,kind,ref,created,state) VALUES(7,'TTEAM:CROOM:100.1','message','e1',1,'done')", [])?;
        c.execute("INSERT INTO thread_inbox(id,session_id,kind,ref,created,state) VALUES(8,'TTEAM:CROOM:100.1','post_refused','1',1,'done')", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let mut reply = post("7:reply", "100.1", "");
    reply.text = "Done.\nCo-Authored-By: Claude <noreply@anthropic.com>".into();
    reply.meta = Some(json!({"owner":"UOWNER","kind":"reply","turn":3}));
    let mut rewrite = post("8:reply", "100.1", "");
    rewrite.text = "Still done.\nCo-Authored-By: Claude <noreply@anthropic.com>".into();
    let mut upload = post("7:upload:0", "100.1", "");
    upload.kind = "upload".into();
    upload.filename = "notes.md".into();
    upload.blob = Some(b"Co-Authored-By: Claude <noreply@anthropic.com>".to_vec());
    for p in [reply, rewrite, upload] {
        outbox::enqueue(&s, p, 1.).await.unwrap();
    }
    let fake = Arc::new(Fake::default());
    let d = dispatcher(&s, fake.clone(), Arc::new(ReplayClock::new(10.)));
    assert_eq!(d.drain_checked(10, None).await.unwrap(), 0);
    let items: Vec<(String, String, String)> = s
        .call(|c| {
            Ok(c.prepare(
                "SELECT kind,ref,payload_json FROM thread_inbox WHERE state='pending' ORDER BY id",
            )?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    // One item, for the refused reply only: not for the refused rewrite (its
    // own inbox item is a post_refused) and not for the upload.
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(
        (items[0].0.as_str(), items[0].1.as_str()),
        ("post_refused", "1")
    );
    let payload: Value = serde_json::from_str(&items[0].2).unwrap();
    assert_eq!(
        payload,
        json!({"outbox_id":1,"code":"egress_ai_trailer","post_kind":"reply","turn":3,"class":"legacy"})
    );
    // Draining again changes nothing: the item is keyed by the post.
    assert_eq!(d.drain_checked(10, None).await.unwrap(), 0);
    let pending: i64 = s
        .call(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM thread_inbox WHERE state='pending'",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(pending, 1);
}
