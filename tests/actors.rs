use fridica::{
    attention::{self, Message},
    config::Attention,
    core::{
        delivery::{AdapterFuture, ClaimedPost, Delivery, DeliveryOutcome},
        parent::{Parent, ParentFailure, ParentRequest},
        time::{ReplayClock, SequenceIds},
        Authority,
    },
    store::Store,
    threads::dispatcher::Dispatcher,
    threads::{
        actor::{Actor, Step},
        controls::{self, Control},
    },
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

const SESSION: &str = "TTEAM:CROOM:100.1";
struct Script {
    requests: Mutex<Vec<ParentRequest>>,
    responses: Mutex<VecDeque<Value>>,
}
impl Script {
    fn new(responses: Vec<Value>) -> Self {
        Self {
            requests: Mutex::new(vec![]),
            responses: Mutex::new(responses.into()),
        }
    }
}
impl Parent for Script {
    fn decide(&self, r: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(r);
            // A scripted `{"failure": code}` is the parent call failing.
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ParentFailure {
                    code: "no_script".into(),
                })
                .and_then(|v| {
                    match v
                        .as_object()
                        .filter(|o| o.len() == 1)
                        .and_then(|o| o.get("failure"))
                        .and_then(Value::as_str)
                    {
                        Some(code) => Err(ParentFailure { code: code.into() }),
                        None => Ok(v),
                    }
                })
        })
    }
}
#[derive(Default)]
struct Sink(Mutex<Vec<ClaimedPost>>);
impl Delivery for Sink {
    fn send(&self, p: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let reference = format!("200.{}", p.id);
            self.0.lock().unwrap().push(p);
            DeliveryOutcome::Sent { reference }
        })
    }
}
async fn intake(s: &Store, n: usize) -> i64 {
    attention::intake(
        s,
        Message {
            files: vec![],
            event_id: format!("e{n}"),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: format!("100.{n}"),
            thread_ts: Some("100.1".into()),
            sender: "UALICE".into(),
            text: "<@UOWNER> help".into(),
            source: "socket".into(),
            meta: None,
            attachments: vec![],
        },
        "UOWNER".into(),
        10.,
        900.,
        format!("o{n}"),
    )
    .await
    .unwrap()
    .unwrap()
}
fn actor<P: Parent>(s: &Store, p: Arc<P>) -> Actor<P> {
    Actor {
        config: None,
        store: Arc::new(s.clone()),
        parent: p,
        clock: Arc::new(ReplayClock::new(20.)),
        ids: Arc::new(SequenceIds::default()),
        owner: "UOWNER".into(),
        limits: Attention::default(),
        observe_only: false,
        parent_timeout: Duration::from_secs(1),
        machine_load: None,
    }
}
async fn scalar(s: &Store, sql: &'static str) -> String {
    s.call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
        .await
        .unwrap()
}

#[tokio::test]
async fn addressed_message_runs_actor_then_delivery_and_closes_its_obligation() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Done.","status":"complete","answers":["o1"]},"summary":"Completed the request"}),
    ]));
    assert_eq!(
        actor(&s, p.clone()).step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(scalar(&s, "SELECT state FROM thread_inbox").await, "done");
    let recorded = scalar(&s, "SELECT response_json FROM parent_turns").await;
    assert_eq!(
        serde_json::from_str::<Value>(&recorded).unwrap()["summary"],
        "Completed the request"
    );
    let sink = Arc::new(Sink::default());
    let d = Dispatcher {
        store: Arc::new(s.clone()),
        delivery: sink.clone(),
        clock: Arc::new(ReplayClock::new(21.)),
        owner: "UOWNER".into(),
        observe_only: false,
        timeout: Duration::from_secs(1),
        heads: None,
    };
    assert_eq!(d.drain(100).await.unwrap(), 1);
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations").await,
        "answered"
    );
    assert_eq!(
        sink.0.lock().unwrap()[0].post.meta.as_ref().unwrap()["owner"],
        "UOWNER"
    );
    assert_eq!(p.requests.lock().unwrap()[0].obligations.len(), 1);
}

#[tokio::test]
async fn observe_only_and_owner_pause_leave_mentions_visible_without_model_or_posts() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![]));
    let mut a = actor(&s, p.clone());
    a.observe_only = true;
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    controls::apply(
        &s,
        SESSION.into(),
        Control::Pause {
            reason: "owner is away".into(),
        },
        Authority::Owner,
        21.,
    )
    .await
    .unwrap();
    intake(&s, 2).await;
    a.observe_only = false;
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert!(p.requests.lock().unwrap().is_empty());
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "0"
    );
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM obligations WHERE state='open'"
        )
        .await,
        "2"
    );
    assert!(
        controls::apply(&s, SESSION.into(), Control::Resume, Authority::System, 22.)
            .await
            .is_err()
    );
    assert!(controls::apply(
        &s,
        SESSION.into(),
        Control::Pause {
            reason: "override".into()
        },
        Authority::System,
        22.
    )
    .await
    .is_err());
    assert_eq!(scalar(&s, "SELECT control FROM threads").await, "paused");
}

struct PausingParent {
    store: Store,
}
impl Parent for PausingParent {
    fn decide(&self, _: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            controls::apply(
                &self.store,
                SESSION.into(),
                Control::Pause {
                    reason: "pause during model call".into(),
                },
                Authority::Owner,
                30.,
            )
            .await
            .unwrap();
            Ok(
                json!({"reply":{"text":"Late reply must not be posted","status":"complete","answers":["o1"]}}),
            )
        })
    }
}
#[tokio::test]
async fn controls_during_a_parent_call_fence_all_actor_effects() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let a = actor(&s, Arc::new(PausingParent { store: s.clone() }));
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Stale);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(scalar(&s, "SELECT state FROM obligations").await, "open");
    assert_eq!(scalar(&s, "SELECT control FROM threads").await, "paused");
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(
        scalar(&s, "SELECT state FROM reply_reservations").await,
        "released"
    );
}

#[tokio::test]
async fn blocked_notice_cannot_answer_but_new_instruction_can_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    s.call(|c| {
        c.execute("UPDATE threads SET status='blocked'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Still blocked","status":"blocked","answers":["o1"]}}),
        json!({"reply":{"text":"Still blocked","status":"blocked"},"dispositions":[{"id":"o1","state":"deferred","until":100.,"reason":"Need credentials"}]}),
        json!({"reopen_blocked":true,"reply":{"text":"New instructions resolved the blocker","status":"complete","answers":["o1","o2"]}}),
    ]));
    let a = actor(&s, p.clone());
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(p.requests.lock().unwrap()[1].call, "repair");
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations WHERE id='o1'").await,
        "deferred"
    );
    intake(&s, 2).await;
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(scalar(&s, "SELECT status FROM threads").await, "complete");
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations WHERE id='o1'").await,
        "awaiting_delivery"
    );
}

#[tokio::test]
async fn waiting_streak_creates_signal_without_pausing_and_parent_failure_settles_blocked() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let p = Arc::new(Script::new(
        (0..3)
            .map(|i| json!({"reply":{"text":format!("Question {i}"),"status":"waiting"}}))
            .collect(),
    ));
    let a = actor(&s, p);
    for n in 1..=3 {
        intake(&s, n).await;
        assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    }
    assert_eq!(scalar(&s, "SELECT control FROM threads").await, "active");
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM obligations WHERE kind='signal'"
        )
        .await,
        "1"
    );
    intake(&s, 4).await;
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        scalar(&s, "SELECT state FROM reply_reservations WHERE inbox_id=4").await,
        "released"
    );
    assert_eq!(
        scalar(&s, "SELECT state FROM thread_inbox WHERE id=4").await,
        "done"
    );
    assert_eq!(scalar(&s, "SELECT status FROM threads").await, "blocked");
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations WHERE id='o4'").await,
        "open"
    );
}

#[tokio::test]
async fn extracted_asks_and_declines_are_committed_with_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![
        json!({"dispositions":[{"id":"o1","state":"declined","reason":"Outside this campaign"}],"asks":[{"summary":"Check the rerun","due":200.}]}),
    ]));
    assert_eq!(
        actor(&s, p).step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations WHERE id='o1'").await,
        "declined"
    );
    assert_eq!(
        scalar(&s, "SELECT summary FROM obligations WHERE kind='ask'").await,
        "Check the rerun"
    );
    assert_eq!(
        scalar(&s, "SELECT state FROM reply_reservations").await,
        "released"
    );
}

#[tokio::test]
async fn uncertain_answers_survive_due_sweeps_and_require_explicit_delivery_recovery() {
    use fridica::store::outbox;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    // Race: a due item already exists when the answer is committed.
    assert_eq!(attention::sweep(&s, 1000.).await.unwrap(), 1);
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Done","status":"complete","answers":["o1"]}}),
        // Even a new mention cannot silently replace the uncertain answer.
        json!({"reply":{"text":"Resending","status":"complete","answers":["o1","o2"]}}),
        json!({"dispositions":[{"id":"o2","state":"deferred","until":2000.,"reason":"Owner must resolve the uncertain delivery"}]}),
    ]));
    let mut a = actor(&s, p.clone());
    a.clock = Arc::new(ReplayClock::new(1001.));
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    let claim = outbox::claim(&s, 1002.).await.unwrap().unwrap();
    outbox::complete(
        &s,
        claim.clone(),
        DeliveryOutcome::Ambiguous {
            code: "timeout".into(),
        },
        "UOWNER".into(),
        1003.,
    )
    .await
    .unwrap();
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(attention::sweep(&s, 1004.).await.unwrap(), 0);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    intake(&s, 2).await;
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        p.requests.lock().unwrap()[1].obligations[0]["deliveries"][0]["state"],
        "ambiguous"
    );
    assert_eq!(p.requests.lock().unwrap()[2].call, "repair");
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );
    assert!(attention::disposition(
        &s,
        "o1".into(),
        attention::Disposition::Deferred {
            reason: "retry".into(),
            until: 2000.
        },
        Authority::System,
        1004.
    )
    .await
    .is_err());
    assert!(outbox::requeue(&s, claim.id, Authority::Owner, 1005.)
        .await
        .unwrap());
    let retry = outbox::claim(&s, 1005.).await.unwrap().unwrap();
    outbox::complete(
        &s,
        retry,
        DeliveryOutcome::Sent {
            reference: "200.1".into(),
        },
        "UOWNER".into(),
        1006.,
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations WHERE id='o1'").await,
        "answered"
    );
}

struct HangingParent(tokio::sync::Notify);
impl Parent for HangingParent {
    fn decide(&self, _: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.0.notify_one();
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn cancelled_actor_recovers_its_claim_and_reuses_reserved_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let parent = Arc::new(HangingParent(tokio::sync::Notify::new()));
    let mut a = actor(&s, parent.clone());
    a.parent_timeout = Duration::from_secs(30);
    let task = tokio::spawn(async move { a.step(SESSION.into()).await });
    tokio::time::timeout(Duration::from_secs(2), parent.0.notified())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        scalar(&s, "SELECT state FROM thread_inbox").await,
        "processing"
    );
    assert_eq!(fridica::threads::actor::recover(&s).await.unwrap(), 1);
    assert_eq!(fridica::threads::actor::recover(&s).await.unwrap(), 0);
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Recovered","status":"complete","answers":["o1"]}}),
    ]));
    assert_eq!(
        actor(&s, p).step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM reply_reservations").await,
        "1"
    );
    // An interrupted external call remains incomplete evidence, not fabricated output.
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE complete=0"
        )
        .await,
        "1"
    );
}

#[tokio::test]
async fn parent_timeout_settles_blocked_and_due_events_do_not_retry_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let mut a = actor(&s, Arc::new(HangingParent(tokio::sync::Notify::new())));
    a.parent_timeout = Duration::from_millis(1);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(scalar(&s, "SELECT json_extract(payload_json,'$.error') FROM replay_events WHERE kind='parent_result'").await, "parent_timeout");
    assert_eq!(scalar(&s, "SELECT state FROM obligations").await, "open");
    assert_eq!(attention::sweep(&s, 1000.).await.unwrap(), 1);
    a.clock = Arc::new(ReplayClock::new(1000.));
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM parent_turns").await,
        "1"
    );
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM obligations WHERE kind='signal'"
        )
        .await,
        "0"
    );
    assert_eq!(attention::sweep(&s, 1000.).await.unwrap(), 0);
}

struct GatedParent {
    entered: tokio::sync::mpsc::UnboundedSender<String>,
    permits: tokio::sync::Semaphore,
}
impl Parent for GatedParent {
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.entered
                .send(request.session["id"].as_str().unwrap().into())
                .unwrap();
            self.permits.acquire().await.unwrap().forget();
            Ok(json!({}))
        })
    }
}
#[tokio::test]
async fn manager_bounds_concurrency_and_preserves_work_for_the_next_pass() {
    use fridica::threads::manager::Manager;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    for n in 1..=3 {
        attention::intake(
            &s,
            Message {
                files: vec![],
                event_id: format!("e{n}"),
                workspace: "TTEAM".into(),
                channel: "CROOM".into(),
                ts: format!("100.{n}"),
                thread_ts: None,
                sender: "UALICE".into(),
                text: "<@UOWNER> help".into(),
                source: "socket".into(),
                meta: None,
                attachments: vec![],
            },
            "UOWNER".into(),
            10.,
            900.,
            format!("o{n}"),
        )
        .await
        .unwrap();
    }
    let (entered, mut events) = tokio::sync::mpsc::unbounded_channel();
    let p = Arc::new(GatedParent {
        entered,
        permits: tokio::sync::Semaphore::new(0),
    });
    let a = Arc::new(actor(&s, p.clone()));
    assert!(Manager::new(a.clone(), 0).is_err());
    let manager = Arc::new(Manager::new(a, 2).unwrap());
    let m = manager.clone();
    let pass = tokio::spawn(async move { m.sweep().await });
    let first = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(first, second);
    assert!(events.try_recv().is_err());
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='processing'"
        )
        .await,
        "2"
    );
    // A competing claim in either thread is refused while its parent is running.
    assert!(attention::claim_due(&s, first, 20.)
        .await
        .unwrap()
        .is_none());
    p.permits.add_permits(2);
    assert_eq!(pass.await.unwrap().unwrap(), 2);
    p.permits.add_permits(1);
    assert_eq!(manager.sweep().await.unwrap(), 1);
    assert_eq!(manager.sweep().await.unwrap(), 0);
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='done'"
        )
        .await,
        "3"
    );
}

#[tokio::test]
async fn owner_resume_reuses_pending_message_and_resets_only_the_intended_history() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    intake(&s, 2).await;
    s.call(|c| {
        c.execute(
            "UPDATE threads SET control='paused',control_json='{}',status='blocked',turns=8",
            [],
        )?;
        c.execute("UPDATE thread_inbox SET not_before=5000 WHERE ref='e2'", [])?;
        Ok(())
    })
    .await
    .unwrap();
    assert!(
        controls::apply(&s, SESSION.into(), Control::Resume, Authority::System, 20.)
            .await
            .is_err()
    );
    controls::apply(&s, SESSION.into(), Control::Resume, Authority::Owner, 20.)
        .await
        .unwrap();
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM thread_inbox").await,
        "2"
    );
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Resumed","status":"complete","answers":["o2"]}}),
    ]));
    let a = actor(&s, p.clone());
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Idle);
    assert_eq!(p.requests.lock().unwrap().len(), 1);
    assert_eq!(
        p.requests.lock().unwrap()[0].trigger["message"]["event_id"],
        "e2"
    );
    assert_eq!(
        scalar(&s, "SELECT trigger_class FROM reply_reservations").await,
        "owner"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(turns AS TEXT) FROM threads").await,
        "1"
    );
}

#[tokio::test]
async fn corrections_bypass_duplicate_suppression_and_acknowledgements_count_as_stalls() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let reply = json!({"reply":{"text":"Waiting.","status":"waiting"}});
    let mut correction = reply.clone();
    correction["note"] = json!({"kind":"correction"});
    let p = Arc::new(Script::new(vec![
        reply.clone(),
        reply,
        correction,
        json!({"reply":{"text":"Understood.","status":"complete"},"note":{"kind":"ack"}}),
    ]));
    let a = actor(&s, p);
    for n in 1..=4 {
        intake(&s, n).await;
        s.call(|c| {
            c.execute("UPDATE messages SET text='Follow-up'", [])?;
            Ok(())
        })
        .await
        .unwrap();
        if n == 1 {
            s.call(|c| {
                c.execute("UPDATE messages SET text='<@UOWNER> help'", [])?;
                Ok(())
            })
            .await
            .unwrap();
        }
        assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    }
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "3"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(no_progress AS TEXT) FROM threads").await,
        "3"
    );
    assert_eq!(scalar(&s, "SELECT control FROM threads").await, "active");
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM obligations WHERE kind='signal'"
        )
        .await,
        "1"
    );
}

#[tokio::test]
async fn invalid_memory_is_repaired_and_note_fault_rolls_back_the_whole_turn() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![
        json!({"note":{"next_step":"x".repeat(1001)}}),
        json!({"reply":{"text":"Done","status":"complete"},"note":{"next_step":"Review"},"context":{"repo":"owner/repo"},"decisions":["Use this"],"summary":"Finished"}),
    ]));
    s.call(|c| {c.execute_batch("CREATE TRIGGER fail_parent_note BEFORE INSERT ON notes BEGIN SELECT RAISE(ABORT,'note fault'); END;")?;Ok(())}).await.unwrap();
    assert_eq!(
        actor(&s, p.clone()).step(SESSION.into()).await.unwrap(),
        Step::Failed
    );
    {
        let requests = p.requests.lock().unwrap();
        assert_eq!(requests[1].call, "repair");
        assert!(requests[1].errors[0].contains("1000"));
    }
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(
        scalar(
            &s,
            "SELECT decisions_json||summary||context_json FROM threads"
        )
        .await,
        "[]{}"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM notes").await,
        "0"
    );
}

#[tokio::test]
async fn suppressing_a_duplicate_blocked_notice_keeps_the_existing_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Blocked.","status":"blocked"},"note":{"blocker":"Need access","assignee":"Alice","next_step":"Grant access"}}),
        json!({"reply":{"text":"Blocked.","status":"blocked"}}),
    ]));
    let a = actor(&s, p);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    s.call(|c| {
        c.execute("UPDATE obligations SET due=20", [])?;
        Ok(())
    })
    .await
    .unwrap();
    attention::sweep(&s, 20.).await.unwrap();
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM notes").await,
        "1"
    );
    assert_eq!(
        scalar(&s, "SELECT json_extract(data_json,'$.assignee') FROM notes").await,
        "Alice"
    );
}

#[tokio::test]
async fn repeat_policy_distinguishes_new_asks_peers_failed_and_uncertain_deliveries() {
    for (sender, text, peer, state, repeat) in [
        ("UALICE", "thanks, noted", false, "sent", false),
        ("UALICE", "still green?", false, "sent", true),
        ("UBOB", "check this too", false, "sent", true),
        ("UALICE", "paste it again", false, "sent", true),
        ("UALICE", "don't repost", false, "sent", false),
        ("UALICE", "hello again", false, "failed", true),
        ("UALICE", "hello again", false, "ambiguous", false),
        ("UALICE", "hello again", false, "pending", false),
        ("UPEER", "still green?", true, "sent", false),
        ("UPEER", "<@UOWNER> repost", true, "sent", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("db")).await.unwrap();
        intake(&s, 1).await;
        // Waiting keeps both peer and human follow-ups out of the triage path.
        let response =
            json!({"reply":{"text":"<@UALICE> <@UPEER> Waiting for CI.","status":"waiting"}});
        let p = Arc::new(Script::new(vec![response.clone(), response]));
        let a = actor(&s, p.clone());
        assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
        s.call(move |c| {
            c.execute("UPDATE outbox SET state=?", [state])?;
            // Exercise pre-v6 rows whose new trigger_event column is empty too.
            c.execute("UPDATE outbox SET trigger_event=''", [])?;
            Ok(())
        })
        .await
        .unwrap();
        intake(&s, 2).await;
        s.call(move |c| {
            c.execute("UPDATE messages SET text=?,sender=?,meta_json=? WHERE event_id='e2'", rusqlite::params![text,sender,peer.then(||json!({"owner":"UPEER","session":SESSION,"turn":1,"status":"waiting","kind":"reply"}).to_string())])?;
            Ok(())
        }).await.unwrap();
        assert_eq!(
            a.step(SESSION.into()).await.unwrap(),
            Step::Committed,
            "{sender} {text} {state}"
        );
        assert_eq!(
            scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            if repeat { "2" } else { "1" },
            "{sender} {text} {state}"
        );
        let evidence: Value = serde_json::from_str(&scalar(&s,"SELECT payload_json FROM replay_events WHERE kind='actor_commit' ORDER BY seq DESC LIMIT 1").await).unwrap();
        assert_eq!(evidence["reply_repeat"]["allowed"], repeat);
        assert_eq!(evidence["reply_repeat"]["previous"]["state"], state);
        assert_eq!(evidence["reply_repeat"]["previous"]["requester"], "UALICE");
        assert_eq!(p.requests.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn owner_resume_can_repeat_a_reply_to_an_unaddressed_followup() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let response = json!({"reply":{"text":"Still waiting.","status":"waiting"}});
    let p = Arc::new(Script::new(vec![response.clone(), response]));
    let a = actor(&s, p);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    controls::apply(
        &s,
        SESSION.into(),
        Control::Pause {
            reason: "Owner review".into(),
        },
        Authority::Owner,
        21.,
    )
    .await
    .unwrap();
    intake(&s, 2).await;
    s.call(|c| {
        c.execute(
            "UPDATE messages SET text='thanks, noted' WHERE event_id='e2'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    controls::apply(&s, SESSION.into(), Control::Resume, Authority::Owner, 22.)
        .await
        .unwrap();
    let mut a = a;
    a.clock = Arc::new(ReplayClock::new(23.));
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "2"
    );
}

#[tokio::test]
async fn unicode_case_equivalent_answers_are_suppressed_and_delivery_is_rechecked_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Straße","status":"waiting"}}),
        json!({"reply":{"text":"STRASSE","status":"waiting"}}),
    ]));
    let a = actor(&s, p);
    for n in 1..=2 {
        intake(&s, n).await;
        if n == 2 {
            s.call(|c| {
                c.execute(
                    "UPDATE messages SET text='thanks, noted' WHERE event_id='e2'",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }
        assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    }
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );

    struct FailedWhileDeciding(Store);
    impl Parent for FailedWhileDeciding {
        fn decide(&self, _: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
            Box::pin(async move {
                // Delivery can settle while the model runs without changing the
                // thread version. Commit must inspect the new delivery state.
                self.0
                    .call(|c| {
                        c.execute("UPDATE outbox SET state='failed'", [])?;
                        Ok(())
                    })
                    .await
                    .unwrap();
                Ok(json!({"reply":{"text":"STRASSE","status":"waiting"}}))
            })
        }
    }
    intake(&s, 3).await;
    s.call(|c| {
        c.execute(
            "UPDATE messages SET text='thanks, noted' WHERE event_id='e3'",
            [],
        )?;
        Ok(())
    })
    .await
    .unwrap();
    let mut updated = actor(&s, Arc::new(FailedWhileDeciding(s.clone())));
    updated.ids = a.ids.clone();
    assert_eq!(updated.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "2"
    );
    assert_eq!(
        scalar(&s, "SELECT state FROM outbox ORDER BY id LIMIT 1").await,
        "failed"
    );
    let evidence:Value=serde_json::from_str(&scalar(&s,"SELECT payload_json FROM replay_events WHERE kind='actor_commit' ORDER BY seq DESC LIMIT 1").await).unwrap();
    assert_eq!(evidence["reply_repeat"]["previous"]["state"], "failed");
    assert_eq!(
        evidence["reply_repeat"]["reason"],
        "previous reply definitely failed"
    );
}

/// Posts a plain message into the thread during its first call.
struct ArrivingParent {
    store: Store,
    requests: Mutex<Vec<ParentRequest>>,
}
impl Parent for ArrivingParent {
    fn decide(&self, r: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            let first = self.requests.lock().unwrap().is_empty();
            self.requests.lock().unwrap().push(r);
            if first {
                attention::intake(
                    &self.store,
                    Message {
                        files: vec![],
                        event_id: "e-late".into(),
                        workspace: "TTEAM".into(),
                        channel: "CROOM".into(),
                        ts: "100.5".into(),
                        thread_ts: Some("100.1".into()),
                        sender: "UBOB".into(),
                        text: "Here is the text you asked for".into(),
                        source: "socket".into(),
                        meta: None,
                        attachments: vec![],
                    },
                    "UOWNER".into(),
                    15.,
                    900.,
                    "o-late".into(),
                )
                .await
                .unwrap();
            }
            Ok(json!({"reply":{"text":"Thanks, read it","status":"complete","answers":["o1"]}}))
        })
    }
}
#[tokio::test]
async fn a_message_arriving_during_the_turn_reruns_it_once_with_the_message() {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let parent = Arc::new(ArrivingParent {
        store: s.clone(),
        requests: Mutex::new(vec![]),
    });
    let a = actor(&s, parent.clone());
    // The first decision is stale: it never saw the message that arrived.
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Stale);
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    let requests = parent.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .history
        .iter()
        .any(|m| m["text"] == "Here is the text you asked for"));
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );
}

#[tokio::test]
async fn a_rewrite_of_a_refused_reply_answers_the_asks_the_refused_one_did() {
    use fridica::store::outbox;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    intake(&s, 1).await;
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Done.\nCo-Authored-By: Claude <noreply@anthropic.com>","status":"complete","answers":["o1"]}}),
        json!({"reply":{"text":"Done.","status":"complete","answers":["o1"]}}),
    ]));
    let a = actor(&s, p.clone());
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    // The gate refuses the post: the ask stays awaiting a delivery that
    // failed, and a rewrite turn is queued.
    let claim = outbox::claim(&s, 11.).await.unwrap().unwrap();
    outbox::complete(
        &s,
        claim,
        DeliveryOutcome::Rejected {
            code: "egress_ai_trailer".into(),
        },
        "UOWNER".into(),
        12.,
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&s, "SELECT kind FROM thread_inbox WHERE state='pending'").await,
        "post_refused"
    );
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    let obligations = p.requests.lock().unwrap()[1].obligations.clone();
    assert_eq!(obligations[0]["state"], "awaiting_delivery");
    assert_eq!(obligations[0]["deliveries"][0]["state"], "failed");
    // The same ask is now answered by the rewrite as well as the refused post.
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(
        scalar(
            &s,
            "SELECT CAST(count(*) AS TEXT) FROM obligation_posts WHERE obligation_id='o1'"
        )
        .await,
        "2"
    );
    assert_eq!(
        scalar(&s, "SELECT text FROM outbox WHERE state='pending'").await,
        "Done."
    );
    // Delivering the rewrite answers the ask even though the first post failed.
    let claim = outbox::claim(&s, 13.).await.unwrap().unwrap();
    outbox::complete(
        &s,
        claim,
        DeliveryOutcome::Sent {
            reference: "200.9".into(),
        },
        "UOWNER".into(),
        14.,
    )
    .await
    .unwrap();
    assert_eq!(
        scalar(&s, "SELECT state FROM obligations").await,
        "answered"
    );
}

/// An externally driven thread (fridica#130): a worker result and a peer's
/// study post are settled with no parent call and no post, and the event
/// feed carries them to the driver as `job_result` and `peer_post`; a human
/// addressing the owner still gets a parent turn, told who drives.
#[tokio::test]
async fn external_threads_settle_results_and_study_posts_for_the_driver() {
    use fridica::{control::events, slack::names::Names};
    use fridica_core::store::Store as _;
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path().join("db")).await.unwrap();
    // A peer's study claim opens the thread.
    attention::intake(
        &s,
        Message {
            files: vec![],
            event_id: "claim".into(),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: "100.1".into(),
            thread_ts: None,
            sender: "UPEER".into(),
            text: "Claim (iteration 1): spectral\napproach: spectral".into(),
            source: "socket".into(),
            meta: Some(json!({"v":2,"owner":"UPEER","session":"x","turn":0,"status":"complete","kind":"study_claim","worker":""})),
            attachments: vec![],
        },
        "UOWNER".into(),
        10.,
        900.,
        "o-claim".into(),
    )
    .await
    .unwrap()
    .unwrap();
    // The driver's delegate: a worker, its finished job and the completion
    // the supervisor records.
    s.transact(|u| {
        assert!(u.set_thread_driver(SESSION, "external", r#"{"kind":"owner"}"#, 11.)?);
        u.add_workers(
            &serde_json::from_value::<Vec<_>>(json!([{"id":"worker-1","session_id":SESSION,
                "machine":"local","workspace":"project","backend":"codex","role":"explorer","ephemeral":true}]))?,
            11.,
        )?;
        u.queue_jobs(
            &serde_json::from_value::<Vec<_>>(json!([{"id":"job-1","worker_id":"worker-1",
                "session_id":SESSION,"brief":"ref: t/g1/i1/Explore/a1/explorer\nExplore.",
                "join_group":"group-1","tags":["t/g1/i1/Explore/a1/explorer"],"context":"fresh"}]))?,
            11.,
        )?;
        u.record("worker_completion", 12., &json!({"job_id":"job-1","attempt":1,"completion":{"interrupted":false,"stopped":false,"allow_retry":false,"artifacts":[],
            "outcome":{"Ok":{"result":{"status":"done","summary":"Two approaches","report":"## Approaches\n- spectral: a"},"backend_session_id":"b"}}}}).to_string(), true)?;
        Ok(())
    })
    .await
    .unwrap();
    s.call(|c| {
        c.execute("UPDATE jobs SET status='done',attempt=1,result_json='{\"status\":\"done\",\"summary\":\"Two approaches\",\"report\":\"r\"}' WHERE id='job-1'",[])?;
        c.execute("INSERT INTO thread_inbox(session_id,kind,ref,created,dedup_key) VALUES(?,'worker_result','job-1',12,'worker-result:job-1')",[SESSION])?;
        Ok(())
    })
    .await
    .unwrap();
    let p = Arc::new(Script::new(vec![]));
    let a = actor(&s, p.clone());
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Idle);
    assert!(p.requests.lock().unwrap().is_empty(), "no parent turn");
    assert_eq!(
        scalar(&s, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(
        scalar(&s, "SELECT CAST(reported AS TEXT) FROM jobs").await,
        "1"
    );
    assert_eq!(
        scalar(&s, "SELECT group_concat(state) FROM thread_inbox").await,
        "done,done"
    );
    assert!(scalar(
        &s,
        "SELECT group_concat(verdict) FROM messages WHERE verdict!=''"
    )
    .await
    .contains("observe: external driver"));
    let feed = s
        .transact(|u| events::read(u, &Names::default(), 0, 1000))
        .await
        .unwrap();
    let kinds: Vec<&str> = feed["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["message", "peer_post", "job", "job_result"]);
    let peer = &feed["events"][1];
    assert_eq!(
        (
            peer["owner"].as_str(),
            peer["meta"]["kind"].as_str(),
            peer["thread"].as_str()
        ),
        (Some("UPEER"), Some("study_claim"), Some("100.1"))
    );
    let result = &feed["events"][3];
    assert_eq!(
        (
            result["join_group"].as_str(),
            result["worker_id"].as_str(),
            result["role"].as_str(),
            result["job_status"].as_str(),
            result["result"]["summary"].as_str()
        ),
        (
            Some("group-1"),
            Some("worker-1"),
            Some("explorer"),
            Some("finished"),
            Some("Two approaches")
        )
    );
    // A human addressing the owner still gets a parent turn; its request
    // says the thread is driven externally.
    intake(&s, 5).await;
    let p = Arc::new(Script::new(vec![
        json!({"reply":{"text":"The study runs on its own.","status":"complete","answers":["o5"]}}),
    ]));
    assert_eq!(
        actor(&s, p.clone()).step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    let requests = p.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].session["driver"], "external");
}

#[tokio::test]
async fn finished_discussion_only_queues_the_final_reply() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    intake(&store, 1).await;
    let parent = Arc::new(Script::new(vec![
        json!({"reply":{"text":"Finished, with checks passed.","status":"complete","discussion":"finished"}}),
    ]));
    let actor = actor(&store, parent.clone());
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Idle);
    assert_eq!(parent.requests.lock().unwrap().len(), 1);
    assert_eq!(
        scalar(
            &store,
            "SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='debrief'"
        )
        .await,
        "0"
    );
    assert_eq!(
        scalar(&store, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );
    assert_eq!(scalar(&store, "SELECT kind FROM outbox").await, "reply");
}

#[tokio::test]
async fn legacy_debrief_inbox_is_settled_without_a_parent_call() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    intake(&store, 1).await;
    store
        .call(|c| {
            c.execute(
                "UPDATE thread_inbox SET kind='debrief',payload_json='{}'",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let parent = Arc::new(Script::new(vec![]));
    let actor = actor(&store, parent.clone());
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Observed);
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Idle);
    assert!(parent.requests.lock().unwrap().is_empty());
    assert_eq!(
        scalar(&store, "SELECT state FROM thread_inbox").await,
        "done"
    );
    assert_eq!(
        scalar(&store, "SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
}

#[tokio::test]
async fn overleaf_dispatch_uses_only_the_target_from_this_thread() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    let pyharp = "111111111111111111111111";
    let moist = "222222222222222222222222";
    let job = |id: &str| {
        json!({
            "context":{"repo":format!("overleaf:{id}")},
            "delegations":[{"brief":format!("Reformat https://www.overleaf.com/project/{id} and push.")}],
            "reply":{"text":"Starting the conversion.","status":"complete"}
        })
    };
    let parent = Arc::new(Script::new(vec![
        job(pyharp),
        job(pyharp),
        job(moist),
        job(pyharp),
        json!({"delegations":[{"brief":format!("Thread-confirmed Overleaf project ID: {moist}\nRead the manuscript.")}],
               "reply":{"text":"Starting.","status":"complete"}}),
    ]));
    let mut actor = actor(&store, parent);
    let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
    actor.config = Some(Arc::new(
        fridica::config::loader::parse(
            corpus["source"].as_str().unwrap(),
            &dir.path().join("config.toml"),
            &fridica::config::LoadContext {
                home: dir.path().into(),
                uid: 1,
                runtime_dir: None,
                protected: vec![],
            },
        )
        .unwrap(),
    ));
    let receiver = fridica::slack::receiver::Receiver::new(
        actor.store.clone(),
        actor.config.clone().unwrap(),
        actor.clock.clone(),
        actor.ids.clone(),
    );
    for (ts, root, text, expected) in [
        (
            "100.1",
            "100.1",
            format!("!f Format <https://www.overleaf.com/project/{pyharp}|Pyharp> to ApJ"),
            "1",
        ),
        ("101.1", "101.1", "!f Re-format that to PRF".into(), "1"),
        (
            "101.2",
            "101.1",
            format!("!f Use https://www.overleaf.com/project/{moist}"),
            "2",
        ),
        (
            "101.3",
            "101.1",
            "!f Update the title in that project".into(),
            "2",
        ),
        ("102.1", "102.1", "!f Check manuscript status".into(), "2"),
    ] {
        let envelope = json!({"type":"events_api","envelope_id":ts,"payload":{"type":"event_callback",
            "team_id":"TTEAM","event_id":ts,"event":{"type":"message","user":"UOWNER","channel":"CROOM",
                "ts":ts,"thread_ts":root,"text":text}}});
        receiver
            .receive(&serde_json::to_vec(&envelope).unwrap())
            .await
            .unwrap();
        assert_eq!(
            actor.step(format!("TTEAM:CROOM:{root}")).await.unwrap(),
            Step::Committed
        );
        assert_eq!(
            scalar(&store, "SELECT CAST(count(*) AS TEXT) FROM jobs").await,
            expected
        );
        if ts == "101.1" {
            assert!(
                scalar(&store, "SELECT text FROM outbox ORDER BY id DESC LIMIT 1")
                    .await
                    .contains("project link")
            );
        }
    }
    assert!(scalar(
        &store,
        "SELECT brief FROM jobs WHERE session_id='TTEAM:CROOM:101.1'"
    )
    .await
    .starts_with(&format!("Thread-confirmed Overleaf project ID: {moist}\n")));
}
