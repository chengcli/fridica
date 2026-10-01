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
    assert!(outbox::requeue(&s, id, Authority::Overseer, 4.)
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
