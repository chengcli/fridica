use fridica::{
    approvals::{rules, Broker},
    config::{loader, registry::Policy, Config, LoadContext},
    core::{
        time::{ReplayClock, SequenceIds},
        worker::*,
        Authority,
    },
    store::{approvals, work, Store},
    threads::controls::{self, Control},
    workers::protocol::ApprovalHandler,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};
use tokio::{sync::mpsc, task::JoinHandle};
const SESSION: &str = "TTEAM:CROOM:100.1";
struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    config: Arc<Config>,
    broker: Arc<Broker>,
    clock: Arc<ReplayClock>,
    worker: WorkerRecord,
    job: Job,
    notifications: mpsc::Receiver<String>,
}
impl Harness {
    async fn new(timeout: f64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        store.call(|c|{c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated) VALUES(?,'TTEAM','CROOM','100.1',1,1)",[SESSION])?;Ok(())}).await.unwrap();
        let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
        let mut config: Config = loader::parse(
            corpus["source"].as_str().unwrap(),
            &dir.path().join("config.toml"),
            &LoadContext {
                home: "/tmp/test-home".into(),
                runtime_dir: None,
                uid: 1,
                protected: vec![],
            },
        )
        .unwrap();
        for m in &mut config.machines.machines {
            for w in &mut m.workspaces {
                w.policy.approval_timeout = timeout;
                w.policy.auto_approve = vec!["pytest".into()];
                w.policy.auto_deny = vec!["rm -rf".into()];
            }
        }
        let config = Arc::new(config);
        let worker:WorkerRecord=serde_json::from_value(json!({"id":"w1","session_id":SESSION,"machine":"gpu","workspace":"unique","backend":"codex"})).unwrap();
        work::add_worker(&store, worker, 1.).await.unwrap();
        let job: Job = serde_json::from_value(
            json!({"id":"j1","worker_id":"w1","session_id":SESSION,"brief":"brief"}),
        )
        .unwrap();
        work::enqueue(&store, job, 1.).await.unwrap();
        let (job, worker) = work::claim(
            &store,
            "j1".into(),
            1,
            config.machines.clone(),
            config.limits.clone(),
            20.,
        )
        .await
        .unwrap()
        .unwrap();
        let clock = Arc::new(ReplayClock::new(20.));
        let (sender, notifications) = mpsc::channel(1);
        let broker = Arc::new(Broker::new(
            Arc::new(store.clone()),
            config.clone(),
            clock.clone(),
            Arc::new(SequenceIds::default()),
            Some(sender),
        ));
        Self {
            _dir: dir,
            store,
            config,
            broker,
            clock,
            worker,
            job,
            notifications,
        }
    }
    fn request(&self, command: &str) -> JoinHandle<ApprovalDecision> {
        let broker = self.broker.clone();
        let worker = self.worker.clone();
        let job = self.job.clone();
        let request = request(command);
        tokio::spawn(async move { broker.request(worker, job, request).await })
    }
    async fn pending(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(2), self.notifications.recv())
            .await
            .unwrap()
            .unwrap()
    }
    async fn worker_status(&self) -> String {
        work::get_worker(&self.store, "w1".into())
            .await
            .unwrap()
            .status
    }
}
fn request(command: &str) -> ApprovalRequest {
    serde_json::from_value(json!({"kind":"command","summary":"private approval text","detail":{"command":command},"backend_request_id":"7"})).unwrap()
}
async fn finish(task: JoinHandle<ApprovalDecision>) -> ApprovalDecision {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap()
}
#[test]
fn frozen_command_rules_match() {
    let corpus: Value =
        serde_json::from_str(include_str!("corpus/approval-instructions.json")).unwrap();
    let policy = Policy {
        auto_approve: vec!["pytest".into(), "git status".into()],
        auto_deny: vec!["rm -rf".into()],
        ..Default::default()
    };
    for case in corpus["rules"].as_array().unwrap() {
        let request: ApprovalRequest = serde_json::from_value(case["request"].clone()).unwrap();
        assert_eq!(
            serde_json::to_value(rules::decide(&policy, &request)).unwrap(),
            case["expected"],
            "{case}"
        );
    }
}
#[tokio::test]
async fn decisions_commit_before_wakeup_and_require_owner_authentication() {
    let mut h = Harness::new(10.).await;
    let task = h.request("make");
    let id = h.pending().await;
    assert_eq!(h.worker_status().await, "awaiting_approval");
    let stored = approvals::get(&h.store, id.clone()).await.unwrap().unwrap();
    assert_eq!(stored.summary, "private approval text");
    assert_eq!(stored.detail, json!({"command":"make"}));
    for authority in [Authority::System, Authority::DesktopReadOnly] {
        assert!(h
            .broker
            .decide(id.clone(), ApprovalDecision::Session, authority)
            .await
            .is_err());
    }
    assert!(h
        .broker
        .decide(id.clone(), ApprovalDecision::Session, Authority::Owner)
        .await
        .unwrap());
    assert!(!h
        .broker
        .decide(id.clone(), ApprovalDecision::Deny, Authority::Owner)
        .await
        .unwrap());
    assert_eq!(finish(task).await, ApprovalDecision::Session);
    let a = approvals::get(&h.store, id.clone()).await.unwrap().unwrap();
    assert_eq!(
        (a.status.as_str(), a.scope.as_str(), a.decided_by.as_str()),
        ("approved", "session", "UOWNER")
    );
    assert_eq!(h.worker_status().await, "running");
    let audit: String = h
        .store
        .call(|c| {
            Ok(
                c.query_row("SELECT group_concat(details_json) FROM audit", [], |r| {
                    r.get(0)
                })?,
            )
        })
        .await
        .unwrap();
    assert!(!audit.contains("private approval text") && !audit.contains("make"));
    assert!(!h
        .broker
        .decide("missing".into(), ApprovalDecision::Once, Authority::Owner)
        .await
        .unwrap());
}
#[tokio::test]
async fn automatic_rules_are_audited_without_pending_rows() {
    let h = Harness::new(10.).await;
    assert_eq!(finish(h.request("pytest -q")).await, ApprovalDecision::Once);
    assert_eq!(
        finish(h.request("rm -rf build && ls")).await,
        ApprovalDecision::Deny
    );
    assert!(approvals::pending(&h.store, 100).await.unwrap().is_empty());
    let actions: Vec<String> = h
        .store
        .call(|c| {
            Ok(c.prepare("SELECT action FROM audit ORDER BY id")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .await
        .unwrap();
    assert_eq!(actions, vec!["approval.allow", "approval.deny"]);
}
#[tokio::test]
async fn timeout_is_durable_and_a_late_owner_decision_cannot_win() {
    let mut h = Harness::new(0.1).await;
    let task = h.request("make");
    let id = h.pending().await;
    assert_eq!(finish(task).await, ApprovalDecision::Deny);
    let a = approvals::get(&h.store, id.clone()).await.unwrap().unwrap();
    assert_eq!(
        (a.status.as_str(), a.decided_by.as_str()),
        ("expired", "timeout")
    );
    assert!(!h
        .broker
        .decide(id, ApprovalDecision::Once, Authority::Owner)
        .await
        .unwrap());
    assert_eq!(h.worker_status().await, "running");
}
#[tokio::test]
async fn injected_clock_deadline_is_checked_in_the_decision_transaction() {
    let mut h = Harness::new(60.).await;
    let task = h.request("make");
    let id = h.pending().await;
    h.clock.set(80.);
    assert!(!h
        .broker
        .decide(id.clone(), ApprovalDecision::Once, Authority::Owner)
        .await
        .unwrap());
    assert_eq!(finish(task).await, ApprovalDecision::Deny);
    assert_eq!(
        approvals::get(&h.store, id).await.unwrap().unwrap().status,
        "expired"
    );
}
#[tokio::test]
async fn dropped_callers_settle_and_do_not_revive_stopped_workers() {
    let mut h = Harness::new(10.).await;
    let task = h.request("make");
    let id = h.pending().await;
    work::stop(&h.store, "w1".into(), 21.).await.unwrap();
    task.abort();
    let _ = task.await;
    h.broker.cancel("w1".into()).await;
    assert_eq!(h.worker_status().await, "stopped");
    assert_eq!(
        approvals::get(&h.store, id).await.unwrap().unwrap().status,
        "cancelled"
    );
    assert_eq!(finish(h.request("pytest")).await, ApprovalDecision::Deny);
}
#[tokio::test]
async fn abort_without_explicit_cancel_is_durable() {
    let mut h = Harness::new(10.).await;
    let task = h.request("make");
    let id = h.pending().await;
    task.abort();
    let _ = task.await;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if approvals::get(&h.store, id.clone())
                .await
                .unwrap()
                .unwrap()
                .status
                == "cancelled"
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.worker_status().await, "running");
}
#[tokio::test]
async fn pause_and_stale_attempts_fence_owner_decisions() {
    for stale in [false, true] {
        let mut h = Harness::new(10.).await;
        let task = h.request("make");
        let id = h.pending().await;
        if stale {
            h.store
                .call(|c| {
                    c.execute("UPDATE jobs SET attempt=attempt+1 WHERE id='j1'", [])?;
                    Ok(())
                })
                .await
                .unwrap();
        } else {
            controls::apply(
                &h.store,
                SESSION.into(),
                Control::Pause {
                    reason: "owner pause".into(),
                },
                Authority::Owner,
                21.,
            )
            .await
            .unwrap();
        }
        assert!(!h
            .broker
            .decide(id, ApprovalDecision::Once, Authority::Owner)
            .await
            .unwrap());
        assert_eq!(finish(task).await, ApprovalDecision::Deny);
        assert_eq!(finish(h.request("pytest")).await, ApprovalDecision::Deny);
    }
}
#[tokio::test]
async fn stop_and_restart_recovery_wake_waiters_without_a_broker_notification() {
    for stop in [false, true] {
        let mut h = Harness::new(10.).await;
        let task = h.request("make");
        h.pending().await;
        if stop {
            work::stop(&h.store, "w1".into(), 21.).await.unwrap();
        } else {
            work::recover(&h.store, 21.).await.unwrap();
        }
        assert_eq!(finish(task).await, ApprovalDecision::Deny);
        assert!(approvals::pending(&h.store, 100).await.unwrap().is_empty());
        assert_ne!(h.worker_status().await, "awaiting_approval");
    }
}
#[tokio::test]
async fn failed_notifications_and_multiple_pending_requests_lose_no_work() {
    let mut h = Harness::new(10.).await;
    h.notifications.close();
    let one = h.request("make one");
    let two = h.request("make two");
    let pending = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let pending = approvals::pending(&h.store, 100).await.unwrap();
            if pending.len() == 2 {
                break pending;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.broker
        .decide(
            pending[0].id.clone(),
            ApprovalDecision::Deny,
            Authority::Owner,
        )
        .await
        .unwrap();
    assert_eq!(h.worker_status().await, "awaiting_approval");
    h.broker.cancel("w1".into()).await;
    assert_eq!(finish(one).await, ApprovalDecision::Deny);
    assert_eq!(finish(two).await, ApprovalDecision::Deny);
    assert_eq!(h.worker_status().await, "running");
}

#[tokio::test]
async fn reconfiguration_cancels_pending_grants_and_changes_rules_atomically() {
    let mut h = Harness::new(10.).await;
    let task = h.request("make");
    let id = h.pending().await;
    let mut config = (*h.config).clone();
    for m in &mut config.machines.machines {
        for w in &mut m.workspaces {
            w.policy.auto_approve.clear();
            w.policy.auto_deny.push("pytest".into());
        }
    }
    h.broker.reconfigure(Arc::new(config)).await.unwrap();
    assert_eq!(finish(task).await, ApprovalDecision::Deny);
    let a = approvals::get(&h.store, id.clone()).await.unwrap().unwrap();
    assert_eq!(a.decided_by, "reconfigured");
    assert!(!h
        .broker
        .decide(id, ApprovalDecision::Once, Authority::Owner)
        .await
        .unwrap());
    assert_eq!(finish(h.request("pytest")).await, ApprovalDecision::Deny);
}

#[tokio::test]
async fn cancellation_during_sqlite_admission_still_settles_the_insert() {
    use fridica::core::time::Identifiers;
    struct SignalledIds(Arc<tokio::sync::Semaphore>);
    impl Identifiers for SignalledIds {
        fn next(&self, _: &str) -> String {
            self.0.add_permits(1);
            "in-flight".into()
        }
    }
    let h = Harness::new(10.).await;
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let broker = Arc::new(Broker::new(
        Arc::new(h.store.clone()),
        h.config.clone(),
        h.clock.clone(),
        Arc::new(SignalledIds(started.clone())),
        None,
    ));
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let store = h.store.clone();
    let blocking = tokio::spawn(async move {
        store
            .call(move |_| {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(3)).unwrap();
                Ok(())
            })
            .await
            .unwrap()
    });
    wait.await.unwrap();
    let worker = h.worker.clone();
    let job = h.job.clone();
    let task = tokio::spawn(async move { broker.request(worker, job, request("make")).await });
    started.acquire().await.unwrap().forget();
    task.abort();
    let _ = task.await;
    release.send(()).unwrap();
    blocking.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(a) = approvals::get(&h.store, "in-flight".into()).await.unwrap() {
                if a.status == "cancelled" {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.worker_status().await, "running");
}
