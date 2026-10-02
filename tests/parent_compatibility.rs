use fridica::{
    attention::{self, Message},
    config::{self, Config, LoadContext},
    core::{
        delivery::*,
        parent::*,
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
    responses: Mutex<VecDeque<Result<Value, ParentFailure>>>,
    calls: Mutex<Vec<ParentRequest>>,
    hook: Mutex<Option<Hook>>,
}
enum Hook {
    Pause(Store),
    Channel(Store),
}
impl Parent for Script {
    fn decide(&self, r: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(r);
            let hook = self.hook.lock().unwrap().take();
            match hook {
                Some(Hook::Pause(store)) => controls::apply(&store,SESSION.into(),Control::Pause{reason:"Owner paused during call".into()},Authority::Owner,21.).await.unwrap(),
                Some(Hook::Channel(store)) => store.call(|c| {c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES('late','TTEAM','CROOM','99.5','99.5','UBOB','arrived during parent call','socket',0)",[])?;Ok(())}).await.unwrap(),
                None => {},
            }
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected parent call")
        })
    }
}
struct Fixture {
    _dir: tempfile::TempDir,
    store: Store,
    config: Arc<Config>,
    parent: Arc<Script>,
    clock: Arc<ReplayClock>,
    ids: Arc<SequenceIds>,
}
impl Fixture {
    async fn new(responses: Vec<Result<Value, ParentFailure>>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let context = LoadContext {
            home: dir.path().join("home"),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        };
        let source=format!("[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.local]\nbackends=['codex']\n[machines.local.policy]\ngpu_confine=false\n[machines.local.workspaces]\nproject={}\n[state]\npath='state.db'\n",json!(dir.path().join("project")));
        let config = Arc::new(
            config::loader::parse(&source, &dir.path().join("config.toml"), &context).unwrap(),
        );
        let store = Store::open(config.state.path.clone()).await.unwrap();
        Self {
            _dir: dir,
            store,
            config,
            parent: Arc::new(Script {
                responses: Mutex::new(responses.into()),
                calls: Mutex::new(vec![]),
                hook: Mutex::new(None),
            }),
            clock: Arc::new(ReplayClock::new(20.)),
            ids: Arc::new(SequenceIds::default()),
        }
    }
    fn actor(&self) -> Actor<Script> {
        Actor {
            config: Some(self.config.clone()),
            store: self.store.clone(),
            parent: self.parent.clone(),
            clock: self.clock.clone(),
            ids: self.ids.clone(),
            owner: "UOWNER".into(),
            limits: Default::default(),
            observe_only: false,
            parent_timeout: Duration::from_secs(2),
            machine_load: None,
        }
    }
    async fn reopen(self) -> Self {
        let Self {
            _dir,
            store,
            config,
            parent,
            clock,
            ids,
        } = self;
        // Closing waits for the lock to be released; dropping would race it.
        store.close().await.unwrap();
        let store = Store::open(config.state.path.clone()).await.unwrap();
        Self {
            _dir,
            store,
            config,
            parent,
            clock,
            ids,
        }
    }
    async fn intake(&self, n: usize) {
        attention::intake(
            &self.store,
            Message {
                event_id: format!("e{n}"),
                workspace: "TTEAM".into(),
                channel: "CROOM".into(),
                ts: format!("100.{n}"),
                thread_ts: Some("100.1".into()),
                sender: "UALICE".into(),
                text: "<@UOWNER> help".into(),
                files: vec![],
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
    async fn scalar(&self, sql: &'static str) -> String {
        self.store
            .call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
}
struct Sink(Mutex<VecDeque<DeliveryOutcome>>);
impl Delivery for Sink {
    fn send(&self, _: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected delivery")
        })
    }
}

#[tokio::test]
async fn details_only_reply_links_the_ordered_upload_before_answer_closure() {
    for outcome in [
        DeliveryOutcome::Sent {
            reference: "201.1".into(),
        },
        DeliveryOutcome::Rejected {
            code: "refused".into(),
        },
        DeliveryOutcome::Ambiguous {
            code: "disconnect".into(),
        },
    ] {
        let sent = matches!(outcome, DeliveryOutcome::Sent { .. });
        // Details this long are a file; shorter ones fold into the message.
        let details = format!("  {}  ", "Full answer. ".repeat(200));
        let f = Fixture::new(vec![Ok(
            json!({"reply":{"text":"  ","details":details,"status":"complete","answers":["o1"]}}),
        )])
        .await;
        f.intake(1).await;
        assert_eq!(
            f.actor().step(SESSION.into()).await.unwrap(),
            Step::Committed
        );
        assert_eq!(
            f.scalar("SELECT text FROM outbox WHERE kind='reply'").await,
            "The reply is in the attached details file."
        );
        assert_eq!(
            f.scalar("SELECT CAST(blob AS TEXT) FROM outbox WHERE kind='upload'")
                .await,
            details.trim()
        );
        assert_eq!(
            f.scalar("SELECT CAST(count(*) AS TEXT) FROM obligation_posts")
                .await,
            "2"
        );
        let dispatcher = Dispatcher {
            store: f.store.clone(),
            delivery: Arc::new(Sink(Mutex::new(
                vec![
                    DeliveryOutcome::Sent {
                        reference: "200.1".into(),
                    },
                    outcome,
                ]
                .into(),
            ))),
            clock: f.clock.clone(),
            owner: "UOWNER".into(),
            observe_only: false,
            timeout: Duration::from_secs(1),
        };
        assert_eq!(dispatcher.drain(1).await.unwrap(), 1);
        assert_eq!(
            f.scalar("SELECT state FROM obligations").await,
            "awaiting_delivery"
        );
        assert_eq!(dispatcher.drain(1).await.unwrap(), usize::from(sent));
        assert_eq!(
            f.scalar("SELECT state FROM obligations").await,
            if sent {
                "answered"
            } else {
                "awaiting_delivery"
            }
        );
    }
}

#[tokio::test]
async fn overflow_and_waiting_mentions_preserve_full_reply_and_protected_spans() {
    let text = format!(
        "UBOB check `UBOB` at https://example.test/UBOB. {}",
        "word ".repeat(1800)
    );
    let f=Fixture::new(vec![Ok(json!({"reply":{"text":text,"details":"Original notes: UBOB","status":"waiting","discussion":"finished"}}))]).await;
    f.intake(1).await;
    f.store
        .call(|c| {
            c.execute("UPDATE messages SET text='<@UOWNER> ask <@UBOB>'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    let post = f.scalar("SELECT text FROM outbox WHERE kind='reply'").await;
    assert!(post.starts_with("<@UALICE> <@UBOB> check `UBOB` at https://example.test/UBOB."));
    assert!(post.chars().count() <= 7000);
    assert!(post.ends_with("_The full reply is in the attached details file._"));
    let details = f
        .scalar("SELECT CAST(blob AS TEXT) FROM outbox WHERE kind='upload'")
        .await;
    assert!(details.starts_with("<@UBOB> check `UBOB`"));
    assert!(details.ends_with("\n\n---\n\nOriginal notes: UBOB"));
    assert_eq!(f.parent.calls.lock().unwrap().len(), 1);
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='debrief'")
            .await,
        "0"
    );
}

#[tokio::test]
async fn first_decision_gets_only_prior_channel_roots_and_repair_keeps_the_snapshot() {
    let f = Fixture::new(vec![
        Ok(json!({"unknown":"repair"})),
        Ok(json!({"reply":{"text":"Done","status":"complete"}})),
        Ok(json!({})),
    ])
    .await;
    f.store.call(|c|{
        for n in 1..=12 {c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES(?,'TTEAM','CROOM',?,?,'UBOB',?,'socket',0)",rusqlite::params![format!("prior{n}"),format!("{n}.0"),format!("{n}.0"),format!("Prior {n}")])?;}
        for (id,ws,ch,ts,root) in [("future","TTEAM","CROOM","200.0","200.0"),("foreign","TTEAM","COTHER","99.0","99.0"),("team","TOTHER","CROOM","99.0","99.0"),("nested","TTEAM","CROOM","99.0","1.0")] {
            c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,source,received_at) VALUES(?,?,?,?,?,'UBOB','must not appear','socket',0)",rusqlite::params![id,ws,ch,ts,root])?;
        } Ok(())
    }).await.unwrap();
    f.intake(1).await;
    *f.parent.hook.lock().unwrap() = Some(Hook::Channel(f.store.clone()));
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    {
        let calls = f.parent.calls.lock().unwrap();
        let channel = &calls[0].session["channel_context"];
        assert_eq!(channel.as_array().unwrap().len(), 10);
        assert_eq!(channel[0]["event_id"], "prior3");
        assert_eq!(channel[9]["event_id"], "prior12");
        assert_eq!(calls[1].session["channel_context"], *channel);
        for kind in ["decide", "repair", "triage", "debrief"] {
            let mut request = calls[0].clone();
            request.call = kind.into();
            let prompt = fridica::parent::prompts::build(&f.config, &request)
                .unwrap()
                .0;
            let data: Value =
                serde_json::from_str(prompt.split("\n\nData:\n").nth(1).unwrap()).unwrap();
            assert_eq!(
                data.get("channel_context").is_some(),
                matches!(kind, "decide" | "repair")
            );
            assert!(data["session"].get("channel_context").is_none());
        }
    }
    f.intake(2).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert!(f.parent.calls.lock().unwrap()[2]
        .session
        .get("channel_context")
        .is_none());
}

#[tokio::test]
async fn unavailable_parent_settles_once_preserves_asks_and_new_instruction_can_reopen() {
    let f=Fixture::new(vec![Err(ParentFailure{code:"parent_exit_7".into()}),Ok(json!({"reopen_blocked":true,"reply":{"text":"Resolved","status":"complete","answers":["o1"]}}))]).await;
    f.intake(1).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(f.scalar("SELECT state FROM thread_inbox").await, "done");
    assert_eq!(f.scalar("SELECT status FROM threads").await, "blocked");
    assert_eq!(f.scalar("SELECT state FROM obligations").await, "open");
    assert_eq!(
        f.scalar("SELECT error FROM parent_turns").await,
        "parent_unavailable"
    );
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM obligation_posts")
            .await,
        "0"
    );
    let f = f.reopen().await;
    f.clock.set(1000.);
    assert_eq!(attention::sweep(&f.store, 1000.).await.unwrap(), 1);
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Observed
    );
    assert_eq!(f.parent.calls.lock().unwrap().len(), 1);
    controls::apply(
        &f.store,
        SESSION.into(),
        Control::Instruct {
            text: "Try with the corrected input".into(),
        },
        Authority::Owner,
        1001.,
    )
    .await
    .unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(f.scalar("SELECT status FROM threads").await, "complete");
    assert_eq!(
        f.scalar("SELECT error FROM parent_turns ORDER BY id DESC LIMIT 1")
            .await,
        ""
    );
    assert_eq!(
        f.scalar("SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
}

#[tokio::test]
async fn invalid_repair_rejects_every_proposed_effect_and_records_both_responses() {
    let raw = json!({"reply":{"text":"Claimed success","status":"complete","answers":["o1"]},"summary":"Must not persist","context":{"branch":"must-not-change"},"delegations":[{"brief":"valid work"},{"brief":"invalid work","machine":"missing"}]});
    let f = Fixture::new(vec![Ok(raw.clone()), Ok(raw.clone())]).await;
    f.intake(1).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(f.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(f.parent.calls.lock().unwrap()[1].previous, Some(raw));
    for table in ["jobs", "workers", "obligation_posts"] {
        let sql = format!("SELECT COUNT(*) FROM {table}");
        assert_eq!(
            f.store
                .call(move |c| Ok(c.query_row(&sql, [], |r| r.get::<_, i64>(0))?))
                .await
                .unwrap(),
            0
        );
    }
    assert_eq!(f.scalar("SELECT summary FROM threads").await, "");
    assert_eq!(f.scalar("SELECT context_json FROM threads").await, "{}");
    assert_eq!(f.scalar("SELECT state FROM obligations").await, "open");
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM parent_turns")
            .await,
        "2"
    );
    assert_eq!(
        f.scalar("SELECT error FROM parent_turns ORDER BY id DESC LIMIT 1")
            .await,
        "parent_invalid_after_repair"
    );
    // Nothing is posted; the owner is asked to review, with both errors kept.
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    // The thread is blocked, which lists it for the owner, with the cause audited.
    assert_eq!(f.scalar("SELECT status FROM threads").await, "blocked");
    assert!(f
        .scalar("SELECT details_json FROM audit WHERE action='parent.blocked'")
        .await
        .contains("parent_invalid_after_repair"));
    for sql in [
        "SELECT json_extract(context_json,'$.validation_error') FROM parent_turns WHERE call='decide'",
        "SELECT json_extract(context_json,'$.validation_error') FROM parent_turns WHERE call='repair'",
    ] {
        let error = f.scalar(sql).await;
        assert!(error.contains("machine"), "{sql}: {error}");
    }
}

#[tokio::test]
async fn recording_faults_and_failed_fallback_commit_do_not_acknowledge_or_post() {
    for fault in [
        "parent_context_recording_failed",
        "parent_recording_failed",
        "parent_context_snapshot_missing",
        "commit",
    ] {
        let f = Fixture::new(vec![Err(ParentFailure {
            code: if fault == "commit" {
                "parent_exit_7"
            } else {
                fault
            }
            .into(),
        })])
        .await;
        f.intake(1).await;
        if fault == "commit" {
            f.store.call(|c|{c.execute_batch("CREATE TEMP TRIGGER fail_fallback BEFORE INSERT ON audit WHEN NEW.action='parent.blocked' BEGIN SELECT RAISE(ABORT,'fixture'); END;")?;Ok(())}).await.unwrap();
        }
        assert_eq!(f.actor().step(SESSION.into()).await.unwrap(), Step::Failed);
        assert_eq!(
            f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            "0"
        );
        assert_eq!(
            f.scalar("SELECT CAST(count(*) AS TEXT) FROM parent_turns")
                .await,
            "0"
        );
        assert_eq!(f.scalar("SELECT state FROM thread_inbox").await, "pending");
        assert_eq!(
            f.scalar("SELECT state FROM obligations WHERE id='o1'")
                .await,
            "open"
        );
    }
}

#[tokio::test]
async fn channel_context_uses_frozen_budget_without_loading_attachment_content() {
    for (length, count) in [(1200, 3), (4100, 1)] {
        let f = Fixture::new(vec![Ok(json!({}))]).await;
        f.store.call(move|c|{
            for n in 1..=10 {
                c.execute("INSERT INTO messages(event_id,workspace,channel,ts,root_ts,sender,text,attachments_json,source,received_at) VALUES(?,'TTEAM','CROOM',?,?,'UBOB',?,?,'socket',0)",rusqlite::params![format!("prior{n}"),format!("{n}.0"),format!("{n}.0"),"x".repeat(length),json!([{"url":"https://private.invalid/never-read"}]).to_string()])?;
            }Ok(())
        }).await.unwrap();
        f.intake(1).await;
        assert_eq!(
            f.actor().step(SESSION.into()).await.unwrap(),
            Step::Committed
        );
        let calls = f.parent.calls.lock().unwrap();
        let context = calls[0].session["channel_context"].as_array().unwrap();
        assert_eq!(context.len(), count);
        assert_eq!(context.last().unwrap()["event_id"], "prior10");
        assert!(!serde_json::to_string(context)
            .unwrap()
            .contains("private.invalid"));
    }
}

#[tokio::test]
async fn fallback_respects_observe_only_and_owner_pause_during_failure() {
    for observe in [true, false] {
        let f = Fixture::new(vec![Err(ParentFailure {
            code: "parent_exit_7".into(),
        })])
        .await;
        f.intake(1).await;
        let mut actor = f.actor();
        actor.observe_only = observe;
        if !observe {
            *f.parent.hook.lock().unwrap() = Some(Hook::Pause(f.store.clone()));
        }
        assert_eq!(
            actor.step(SESSION.into()).await.unwrap(),
            if observe { Step::Observed } else { Step::Stale }
        );
        assert_eq!(
            f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            "0"
        );
        assert_eq!(f.scalar("SELECT state FROM obligations").await, "open");
        assert_eq!(f.parent.calls.lock().unwrap().len(), usize::from(!observe));
        if !observe {
            assert_eq!(f.scalar("SELECT control FROM threads").await, "paused");
            assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Observed);
            assert_eq!(f.parent.calls.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn blank_replies_are_quiet_but_cannot_answer_obligations() {
    let f = Fixture::new(vec![
        Ok(json!({"reply":{"text":" \n","details":" ","status":"complete","answers":["o1"]}})),
        Ok(json!({"reply":{"text":" ","status":"complete"}})),
    ])
    .await;
    f.intake(1).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(f.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(f.scalar("SELECT state FROM obligations").await, "open");
}

#[tokio::test]
async fn infrastructure_failure_signals_do_not_spawn_recursive_signals() {
    let error = || {
        Err(ParentFailure {
            code: "parent_recording_failed".into(),
        })
    };
    let f = Fixture::new(vec![error(), error()]).await;
    f.intake(1).await;
    assert_eq!(f.actor().step(SESSION.into()).await.unwrap(), Step::Failed);
    assert_eq!(attention::sweep(&f.store, 20.).await.unwrap(), 1);
    assert_eq!(f.actor().step(SESSION.into()).await.unwrap(), Step::Failed);
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM obligations WHERE kind='signal'")
            .await,
        "1"
    );
    assert_eq!(attention::sweep(&f.store, 20.).await.unwrap(), 0);
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
}

#[tokio::test]
async fn held_due_work_survives_failure_and_runs_after_successful_owner_reopening() {
    let f=Fixture::new(vec![Err(ParentFailure{code:"parent_exit_7".into()}),Ok(json!({"reopen_blocked":true,"reply":{"text":"Ready to review outstanding asks","status":"complete"}})),Ok(json!({"reply":{"text":"Answer","status":"complete","answers":["o1"]}}))]).await;
    f.intake(1).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    f.clock.set(1000.);
    attention::sweep(&f.store, 1000.).await.unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Observed
    );
    assert_eq!(
        f.scalar("SELECT state FROM thread_inbox WHERE kind='obligation_due'")
            .await,
        "pending"
    );
    assert_eq!(f.parent.calls.lock().unwrap().len(), 1);
    controls::apply(
        &f.store,
        SESSION.into(),
        Control::Instruct {
            text: "Backend fixed; continue reviewing".into(),
        },
        Authority::Owner,
        1000.,
    )
    .await
    .unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    f.clock.set(1061.);
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(
        f.parent.calls.lock().unwrap()[2].trigger["kind"],
        "obligation_due"
    );
    assert_eq!(
        f.scalar("SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='obligation_due'")
            .await,
        "1"
    );
}

#[tokio::test]
async fn failed_parent_turns_post_nothing_and_a_new_mention_retries_the_parent() {
    let f = Fixture::new(vec![
        Err(ParentFailure {
            code: "parent_timeout".into(),
        }),
        Err(ParentFailure {
            code: "parent_exit_7".into(),
        }),
    ])
    .await;
    let actor = f.actor();
    f.intake(1).await;
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Committed);
    // A new request in the blocked thread tries the parent again; neither
    // failure is posted, each keeps its own cause, and both asks stay open.
    f.intake(2).await;
    assert_eq!(actor.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(f.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(f.scalar("SELECT status FROM threads").await, "blocked");
    assert_eq!(
        f.scalar(
            "SELECT group_concat(json_extract(context_json,'$.failure'),',') FROM parent_turns"
        )
        .await,
        "parent_timeout,parent_exit_7"
    );
    assert_eq!(
        f.scalar("SELECT CAST(count(*) AS TEXT) FROM obligations WHERE state='open'")
            .await,
        "2"
    );
}

#[tokio::test]
async fn owner_closed_asks_retire_held_due_events_without_another_model_call() {
    let f = Fixture::new(vec![Err(ParentFailure {
        code: "parent_exit_7".into(),
    })])
    .await;
    f.intake(1).await;
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    f.clock.set(1000.);
    attention::sweep(&f.store, 1000.).await.unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Observed
    );
    attention::disposition(
        &f.store,
        "o1".into(),
        attention::Disposition::OwnerClosed {
            reason: "Resolved independently".into(),
        },
        Authority::Owner,
        1001.,
    )
    .await
    .unwrap();
    f.clock.set(1061.);
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Observed
    );
    assert_eq!(
        f.scalar("SELECT state FROM thread_inbox WHERE kind='obligation_due'")
            .await,
        "done"
    );
    assert_eq!(f.parent.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn waiting_worker_result_addresses_the_owner_who_requested_the_work() {
    let f = Fixture::new(vec![Ok(
        json!({"reply":{"text":"Which option should I use?","status":"waiting"}}),
    )])
    .await;
    f.intake(1).await;
    // A completed job whose originating instruction came through owner controls.
    f.store.call(|c| {
        c.execute("UPDATE thread_inbox SET state='done'", [])?;
        c.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,created,state,dedup_key) VALUES(?,'owner_instruction','instruction','{}',10,'done','owner-origin')", [SESSION])?;
        let origin = c.last_insert_rowid();
        c.execute("INSERT INTO workers(id,session_id,machine,workspace,backend,created,updated) VALUES('worker',?,'local','project','codex',10,10)", [SESSION])?;
        c.execute("INSERT INTO jobs(id,worker_id,session_id,brief,status,result_json,inbox_id,queued_at) VALUES('job','worker',?,'Review options','done','{}',?,10)", rusqlite::params![SESSION,origin])?;
        c.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,created,dedup_key) VALUES(?,'worker_result','job','{}',11,'job-result')", [SESSION])?;
        Ok(())
    }).await.unwrap();
    assert_eq!(
        f.actor().step(SESSION.into()).await.unwrap(),
        Step::Committed
    );
    assert_eq!(
        f.scalar("SELECT text FROM outbox WHERE kind='report'")
            .await,
        "<@UOWNER> Which option should I use?"
    );
}
