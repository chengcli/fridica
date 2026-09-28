use fridica::{
    attention::Message,
    config::{loader, Config, LoadContext},
    core::{
        delivery::*,
        parent::*,
        time::{ReplayClock, SequenceIds},
        worker::*,
        Authority,
    },
    store::{work, Store},
    threads::{
        controls::Control,
        runtime::{Adapters, Runtime},
    },
    workers::protocol::*,
};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Semaphore;
const SESSION: &str = "TTEAM:CROOM:100.1";
#[derive(Default)]
struct ParentScript {
    calls: Mutex<Vec<ParentRequest>>,
    responses: Mutex<VecDeque<Value>>,
}
impl Parent for ParentScript {
    fn decide(&self, r: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(r);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ParentFailure {
                    code: "script_exhausted".into(),
                })
        })
    }
}
#[derive(Default)]
struct Sink {
    calls: Mutex<Vec<ClaimedPost>>,
    outcomes: Mutex<VecDeque<DeliveryOutcome>>,
}
impl Delivery for Sink {
    fn send(&self, p: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let reference = format!("200.{}", p.id);
            self.calls.lock().unwrap().push(p);
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DeliveryOutcome::Sent { reference })
        })
    }
}
struct WorkerScript {
    calls: Mutex<Vec<RunRequest>>,
    outcomes: Mutex<VecDeque<Result<Outcome, WorkerFailure>>>,
    release: Semaphore,
}
impl Default for WorkerScript {
    fn default() -> Self {
        Self {
            calls: Mutex::new(vec![]),
            outcomes: Mutex::new(VecDeque::new()),
            release: Semaphore::new(0),
        }
    }
}
struct Fake {
    script: Arc<WorkerScript>,
    alive: AtomicBool,
}
impl Worker for Fake {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn busy(&self) -> bool {
        false
    }
    fn run(
        &self,
        r: RunRequest,
        _: WorkerRecord,
        _: Job,
        _: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>> {
        Box::pin(async move {
            self.script.calls.lock().unwrap().push(r);
            self.script.release.acquire().await.unwrap().forget();
            self.script.outcomes.lock().unwrap().pop_front().unwrap_or_else(||Ok(Outcome{result:serde_json::from_value(json!({"status":"done","summary":"Checks passed","report":"Checks passed."})).unwrap(),backend_session_id:"backend-1".into()}))
        })
    }
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
    }
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async {
            self.alive.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Fakes(Arc<WorkerScript>);
impl Factory for Fakes {
    fn instructions(&self, _: &Config, _: &WorkerRecord) -> anyhow::Result<String> {
        Ok("Scripted test instructions".into())
    }
    fn create(&self, _: WorkerSpec) -> Result<Arc<dyn Worker>, WorkerFailure> {
        Ok(Arc::new(Fake {
            script: self.0.clone(),
            alive: AtomicBool::new(true),
        }))
    }
}
struct Harness {
    dir: tempfile::TempDir,
    store: Store,
    config: Arc<Config>,
    clock: Arc<ReplayClock>,
    parent: Arc<ParentScript>,
    worker: Arc<WorkerScript>,
    sink: Arc<Sink>,
    runtime: Runtime<ParentScript, Sink>,
}
impl Harness {
    async fn new(responses: Vec<Value>, observe: bool) -> Self {
        Self::with_io(responses, observe, Arc::new(NoJobIo)).await
    }
    async fn with_io(responses: Vec<Value>, observe: bool, job_io: Arc<dyn JobIo>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let config = loader::parse(
            &format!(
                r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local]
backends=["codex"]
max_jobs=2
max_workers=2
[machines.local.workspaces]
project="{}"
[state]
path="{}"
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
        .unwrap();
        let config = Arc::new(config);
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = Arc::new(ReplayClock::new(20.));
        let parent = Arc::new(ParentScript::default());
        parent.responses.lock().unwrap().extend(responses);
        let worker = Arc::new(WorkerScript::default());
        let sink = Arc::new(Sink::default());
        let runtime = Runtime::start(
            store.clone(),
            config.clone(),
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(Fakes(worker.clone())),
                job_io,
            },
            clock.clone(),
            Arc::new(SequenceIds::default()),
            observe,
        )
        .await
        .unwrap();
        Self {
            dir,
            store,
            config,
            clock,
            parent,
            worker,
            sink,
            runtime,
        }
    }
    async fn intake(&self, peer: bool) {
        let message = Message {
            files: vec![],
            event_id: "e1".into(),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: "100.1".into(),
            thread_ts: None,
            sender: if peer { "UPEER" } else { "UALICE" }.into(),
            text: "<@UOWNER> run checks".into(),
            source: "socket".into(),
            meta: if peer {
                Some(json!({"owner":"UPEER","kind":"reply","status":"complete","turn":1}))
            } else {
                None
            },
            attachments: vec![],
        };
        assert!(self
            .runtime
            .intake(message.clone())
            .await
            .unwrap()
            .is_some());
        assert!(self.runtime.intake(message).await.unwrap().is_none());
    }
    async fn scalar(&self, sql: &str) -> String {
        let sql = sql.to_owned();
        self.store
            .call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
    async fn finish(&self, n: usize, total: usize) {
        self.worker.release.add_permits(n);
        tokio::time::timeout(Duration::from_secs(3),async{
            loop{if self.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs WHERE status NOT IN ('queued','running')").await==total.to_string(){break;}tokio::time::sleep(Duration::from_millis(1)).await;}
        }).await.unwrap();
    }
}
fn delegate() -> Value {
    json!({"reply":{"text":"Running checks.","status":"complete"},"delegations":[{"brief":"Run focused checks","machine":"local","workspace":"project"}]})
}

#[tokio::test]
async fn intake_to_job_to_confirmed_report_is_deterministic_and_records_boundaries() {
    async fn run(tape: Option<&Value>) -> Value {
        let responses = tape
            .map(|t| {
                t["events"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|e| e["kind"] == "parent_result")
                    .map(|e| e["payload"]["response"].clone())
                    .collect()
            })
            .unwrap_or_else(|| vec![delegate()]);
        let h = Harness::new(responses, false).await;
        if let Some(tape) = tape {
            for e in tape["events"].as_array().unwrap() {
                match e["kind"].as_str().unwrap() {
                    "intake" => {
                        h.runtime
                            .intake(
                                serde_json::from_value(e["payload"]["message"].clone()).unwrap(),
                            )
                            .await
                            .unwrap();
                    }
                    "worker_completion" => {
                        let completion: work::Completion =
                            serde_json::from_value(e["payload"]["completion"].clone()).unwrap();
                        h.worker
                            .outcomes
                            .lock()
                            .unwrap()
                            .push_back(completion.outcome);
                    }
                    "delivery" => h.sink.outcomes.lock().unwrap().push_back(
                        serde_json::from_value(e["payload"]["raw_result"].clone()).unwrap(),
                    ),
                    _ => {}
                }
            }
        } else {
            h.intake(false).await;
        }
        let first = h.runtime.pass().await.unwrap();
        assert_eq!((first.turns, first.started, first.delivered), (1, 1, 1));
        assert_eq!(h.scalar("SELECT status FROM threads").await, "working");
        assert_eq!(h.scalar("SELECT state FROM obligations").await, "open");
        h.finish(1, 1).await;
        let last = h.runtime.pass().await.unwrap();
        assert_eq!((last.turns, last.started, last.delivered), (1, 0, 1));
        assert_eq!(h.scalar("SELECT state FROM obligations").await, "answered");
        assert_eq!(
            h.scalar("SELECT CAST(reported AS TEXT) FROM jobs").await,
            "1"
        );
        assert_eq!(h.scalar("SELECT status FROM threads").await, "complete");
        assert_eq!(
            h.parent.calls.lock().unwrap().len(),
            1,
            "single report must bypass parent"
        );
        assert_eq!(h.runtime.pass().await.unwrap().delivered, 0);
        let events:Vec<Value>=h.store.call(|c|{let rows:Vec<String>=c.prepare("SELECT json_object('kind',kind,'time',time,'payload',json(payload_json),'complete',complete) FROM replay_events ORDER BY seq")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;Ok(rows.iter().map(|s|serde_json::from_str(s).unwrap()).collect())}).await.unwrap();
        for kind in [
            "parent_call",
            "parent_result",
            "worker_call",
            "worker_completion",
            "delivery_call",
            "delivery",
        ] {
            assert!(events.iter().any(|r| r["kind"] == kind), "missing {kind}");
        }
        assert!(events.iter().all(|r| r["complete"] == 1));
        let posts = h.sink.calls.lock().unwrap().clone();
        let projection = json!({"events":events,"posts":posts,"workers":work::snapshot(&h.store).await.unwrap()});
        h.runtime.close().await.unwrap();
        serde_json::from_str(
            &projection
                .to_string()
                .replace(h.dir.path().to_str().unwrap(), "__ROOT__"),
        )
        .unwrap()
    }
    let captured = run(None).await;
    assert_eq!(captured, run(Some(&captured)).await);
}

#[tokio::test]
async fn grouped_results_wait_for_last_completion_then_report_once() {
    let mut d = delegate();
    d["delegations"]
        .as_array_mut()
        .unwrap()
        .push(json!({"brief":"Independent review","role":"reviewer","ephemeral":true}));
    let h=Harness::new(vec![d,json!({"reply":{"text":"Both checks passed.","status":"complete","answers":["obligation-0000000000000001"]}})],false).await;
    h.intake(false).await;
    assert_eq!(h.runtime.pass().await.unwrap().started, 2);
    h.finish(1, 1).await;
    h.runtime.pass().await.unwrap();
    assert_eq!(h.parent.calls.lock().unwrap().len(), 1);
    assert_eq!(h.sink.calls.lock().unwrap().len(), 1);
    h.finish(1, 2).await;
    h.runtime.pass().await.unwrap();
    {
        let calls = h.parent.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].trigger["results"].as_array().unwrap().len(), 2);
    }
    assert_eq!(
        h.scalar("SELECT CAST(sum(reported) AS TEXT) FROM jobs")
            .await,
        "2"
    );
    assert_eq!(h.scalar("SELECT state FROM obligations").await, "answered");
    assert_eq!(h.runtime.pass().await.unwrap().turns, 0);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn owner_pause_retains_result_and_resume_does_not_delegate_again() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.runtime
        .control(
            SESSION.into(),
            Control::Pause {
                reason: "Owner review".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(reported AS TEXT) FROM jobs").await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT state FROM thread_inbox WHERE kind='worker_result'")
            .await,
        "pending"
    );
    assert!(h
        .runtime
        .control(SESSION.into(), Control::Resume, Authority::Overseer)
        .await
        .is_err());
    h.runtime
        .control(SESSION.into(), Control::Resume, Authority::Owner)
        .await
        .unwrap();
    assert_eq!(h.runtime.pass().await.unwrap().delivered, 1);
    assert_eq!(h.parent.calls.lock().unwrap().len(), 1);
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "1"
    );
    assert_eq!(h.scalar("SELECT state FROM obligations").await, "answered");
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn observe_only_has_no_parent_worker_or_delivery_effects() {
    let h = Harness::new(vec![delegate()], true).await;
    let event = json!({"type":"events_api","envelope_id":"live-event","payload":{
        "type":"event_callback","team_id":"TTEAM","event_id":"e1",
        "event":{"type":"message","channel":"CROOM","ts":"100.1","user":"UALICE","text":"<@UOWNER> help"}
    }});
    assert!(h
        .runtime
        .slack_receiver()
        .receive(&serde_json::to_vec(&event).unwrap())
        .await
        .unwrap()
        .is_some());
    let progress = h.runtime.pass().await.unwrap();
    assert_eq!((progress.started, progress.delivered), (0, 0));
    assert!(h.parent.calls.lock().unwrap().is_empty());
    assert!(h.worker.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
    assert_eq!(h.scalar("SELECT state FROM obligations").await, "open");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "0"
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn invalid_placement_repairs_before_effects_and_job_insert_failure_rolls_back_reply() {
    let mut invalid = delegate();
    invalid["delegations"][0]["machine"] = json!("missing");
    let h = Harness::new(vec![invalid, delegate()], false).await;
    h.intake(false).await;
    h.store.call(|c|{c.execute_batch("CREATE TEMP TRIGGER fail_job BEFORE INSERT ON jobs BEGIN SELECT RAISE(ABORT,'injected job failure'); END;")?;Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(h.parent.calls.lock().unwrap()[1].call, "repair");
    for table in ["workers", "jobs", "outbox", "parent_turns"] {
        assert_eq!(
            h.scalar(&format!("SELECT CAST(count(*) AS TEXT) FROM {table}"))
                .await,
            "0"
        );
    }
    assert_eq!(h.scalar("SELECT state FROM thread_inbox").await, "pending");
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn ambiguous_report_is_not_resent_and_does_not_close_the_ask() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.sink
        .outcomes
        .lock()
        .unwrap()
        .push_back(DeliveryOutcome::Ambiguous {
            code: "connection_lost".into(),
        });
    h.runtime.pass().await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(
        h.scalar("SELECT state FROM outbox WHERE kind='report'")
            .await,
        "ambiguous"
    );
    assert_eq!(h.sink.calls.lock().unwrap().len(), 2);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn long_direct_report_closes_only_after_the_full_report_upload() {
    let h = Harness::new(vec![delegate()], false).await;
    h.worker.outcomes.lock().unwrap().push_back(Ok(Outcome {
        result: serde_json::from_value(
            json!({"status":"done","summary":"done","report":"x".repeat(8000)}),
        )
        .unwrap(),
        backend_session_id: "b".into(),
    }));
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.sink.outcomes.lock().unwrap().extend([
        DeliveryOutcome::Sent {
            reference: "200.2".into(),
        },
        DeliveryOutcome::Ambiguous {
            code: "upload_timeout".into(),
        },
    ]);
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM obligation_posts")
            .await,
        "2"
    );
    assert_eq!(
        h.sink.calls.lock().unwrap()[2]
            .post
            .blob
            .as_ref()
            .unwrap()
            .len(),
        8000
    );
    assert_eq!(h.parent.calls.lock().unwrap().len(), 1);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn restart_recovers_unconsumed_results_and_uncertain_sends_without_duplication() {
    for uncertain in [false, true] {
        let h = Harness::new(vec![delegate()], false).await;
        h.intake(false).await;
        h.runtime.pass().await.unwrap();
        h.finish(1, 1).await;
        if uncertain {
            // Commit the report but interrupt its send before acknowledgement.
            let actor = fridica::threads::actor::Actor {
                store: h.store.clone(),
                config: Some(h.config.clone()),
                parent: h.parent.clone(),
                clock: h.clock.clone(),
                ids: Arc::new(SequenceIds::default()),
                owner: "UOWNER".into(),
                limits: h.config.attention.clone(),
                observe_only: false,
                parent_timeout: Duration::from_secs(1),
            };
            actor.step(SESSION.into()).await.unwrap();
            assert!(fridica::store::outbox::claim(&h.store, 20.)
                .await
                .unwrap()
                .is_some());
        }
        h.runtime.close().await.unwrap();
        let Harness {
            dir,
            store,
            config,
            clock,
            parent,
            worker,
            sink,
            runtime,
        } = h;
        drop(runtime);
        drop(store);
        let store = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match Store::open(dir.path().join("db")).await {
                    Ok(s) => break s,
                    Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await
        .unwrap();
        let runtime = Runtime::start(
            store.clone(),
            config,
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(Fakes(worker.clone())),
                job_io: Arc::new(NoJobIo),
            },
            clock,
            Arc::new(fridica::core::time::RandomIds),
            false,
        )
        .await
        .unwrap();
        let progress = runtime.pass().await.unwrap();
        assert_eq!(progress.started, 0);
        assert_eq!(progress.delivered, usize::from(!uncertain));
        assert_eq!(parent.calls.lock().unwrap().len(), 1);
        assert_eq!(worker.calls.lock().unwrap().len(), 1);
        assert_eq!(fridica::threads::actor::recover(&store).await.unwrap(), 0);
        assert_eq!(work::recover(&store, 20.).await.unwrap(), 0);
        assert_eq!(
            fridica::store::outbox::recover(&store, 20.).await.unwrap(),
            0
        );
        let state: String = store
            .call(|c| Ok(c.query_row("SELECT state FROM obligations", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(
            state,
            if uncertain {
                "awaiting_delivery"
            } else {
                "answered"
            }
        );
        runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn peer_result_throttling_persists_and_does_not_block_owner_instructions() {
    let h = Harness::new(vec![delegate(), json!({})], false).await;
    h.intake(true).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    // Fill the other five peer slots with confirmed historical reservations.
    h.store.call(|c|{for n in 0..5{
        c.execute("INSERT INTO thread_inbox(session_id,kind,created,state) VALUES(?,'message',20,'done')",[SESSION])?;let inbox=c.last_insert_rowid();
        c.execute("INSERT INTO outbox(idem_key,session_id,kind,channel,thread_ts,text,created,state,delivered_at) VALUES(?,?,'reply','CROOM','100.1','historical',20,'sent',20)",rusqlite::params![format!("historic-{n}"),SESSION])?;
        let post=c.last_insert_rowid();
        c.execute("INSERT INTO reply_reservations(id,session_id,inbox_id,outbox_id,trigger_class,reserved_at,state) VALUES(?,?,?,?,'peer',20,'sent')",rusqlite::params![format!("occupied-{n}"),SESSION,inbox,post])?;
    }Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT state FROM thread_inbox WHERE kind='worker_result'")
            .await,
        "pending"
    );
    assert_eq!(
        h.scalar("SELECT CAST(reported AS TEXT) FROM jobs").await,
        "0"
    );
    assert_eq!(h.sink.calls.lock().unwrap().len(), 1);
    h.runtime
        .control(
            SESSION.into(),
            Control::Instruct {
                text: "Keep this request visible".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    // First pass encounters the rate-limited result; the next can process the
    // owner instruction despite the earlier result's durable deferral.
    h.runtime.pass().await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(
        h.parent.calls.lock().unwrap()[1].trigger["kind"],
        "owner_instruction"
    );
    h.clock.set(3621.);
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT state FROM obligations WHERE kind='mention'")
            .await,
        "answered"
    );
    assert_eq!(
        h.scalar("SELECT trigger_class FROM outbox WHERE kind='report'")
            .await,
        "peer"
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn frozen_python_flows_match_declared_projection_with_scripted_attention() {
    let corpus: Value = serde_json::from_str(include_str!("corpus/flow.json")).unwrap();
    for fixture in corpus["fixtures"].as_array().unwrap() {
        let events = fixture["events"].as_array().unwrap();
        let mut responses = Vec::new();
        for event in events.iter().filter(|e| e["kind"] == "parent") {
            let original = &event["response"];
            let mut response = json!({"reply":{"text":original["reply"]["text"],"details":original["reply"]["details"],"status":original["reply"]["status"]},"delegations":original["delegate"]});
            if !responses.is_empty()
                && fixture["rust_attention"]["grouped_answers_original_mention"] == true
            {
                response["reply"]["answers"] = json!(["obligation-0000000000000001"]);
            }
            responses.push(response);
        }
        let h = Harness::new(responses, fixture["name"] == "observe").await;
        for event in events.iter().filter(|e| e["kind"] == "worker") {
            h.worker.outcomes.lock().unwrap().push_back(Ok(Outcome {
                result: serde_json::from_value(event["response"].clone()).unwrap(),
                backend_session_id: "script-session".into(),
            }));
        }
        for event in events.iter().filter(|e| e["kind"] == "delivery") {
            h.sink
                .outcomes
                .lock()
                .unwrap()
                .push_back(DeliveryOutcome::Sent {
                    reference: event["response"].as_str().unwrap().into(),
                });
        }
        h.intake(false).await;
        let started = h.runtime.pass().await.unwrap().started;
        if started > 0 {
            h.finish(started, started).await;
            h.runtime.pass().await.unwrap();
        }
        let actual=h.store.call(|c|{
            let mut projection=json!({});
            for (name,sql) in [
                ("jobs","SELECT json_object('brief',brief,'status',status,'attempt',attempt,'reported',reported,'join_group',join_group,'deliverable',deliverable,'fetch_repo',fetch_repo,'fetch_ref',fetch_ref) FROM jobs ORDER BY rowid"),
                ("workers","SELECT json_object('machine',machine,'workspace',workspace,'backend',backend,'role',role,'ephemeral',ephemeral,'status',status,'slot',slot) FROM workers ORDER BY rowid"),
                ("inbox","SELECT json_object('kind',kind,'state',state) FROM thread_inbox ORDER BY id"),
                ("outbox","SELECT json_object('kind',kind,'text',text,'state',state,'attempts',attempts) FROM outbox ORDER BY id"),
                ("threads","SELECT json_object('status',status,'control',control,'turns',turns,'wait_streak',wait_streak,'no_progress',no_progress,'context',json(context_json)) FROM threads ORDER BY id")
            ] {
                let rows:Vec<String>=c.prepare(sql)?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
                projection[name]=json!(rows.iter().map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>());
            }
            let verdicts:Vec<String>=c.prepare("SELECT verdict FROM messages WHERE source!='self' ORDER BY id")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
            projection["verdicts"]=json!(verdicts.iter().map(|s|s.split(':').next().unwrap()).collect::<Vec<_>>());
            // Frozen DTOs serialize empty context defaults; v6 stores only set
            // fields. Compare their effective values, with no field exclusions.
            for thread in projection["threads"].as_array_mut().unwrap(){
                for key in ["machine","workspace","repo","branch","backend"]{
                    if thread["context"][key].is_null(){thread["context"][key]=json!("");}
                }
            }
            Ok(projection)
        }).await.unwrap();
        assert_eq!(actual, fixture["expected"], "{}", fixture["name"]);
        assert_eq!(
            h.parent.calls.lock().unwrap().len(),
            events.iter().filter(|e| e["kind"] == "parent").count()
        );
        h.runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn execution_retry_keeps_session_and_produces_one_final_report() {
    let h = Harness::new(vec![delegate()], false).await;
    h.worker
        .outcomes
        .lock()
        .unwrap()
        .push_back(Err(WorkerFailure {
            kind: Failure::Execution,
            code: "temporary_failure".into(),
            backend_session_id: "resume-this".into(),
        }));
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.worker.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if h.scalar("SELECT status FROM jobs").await == "queued" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='worker_result'")
            .await,
        "0"
    );
    assert_eq!(h.runtime.pass().await.unwrap().started, 1);
    h.finish(1, 1).await;
    h.runtime.pass().await.unwrap();
    {
        let calls = h.worker.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].resume, "resume-this");
        assert_eq!(calls[0].job_id, calls[1].job_id);
    }
    assert_eq!(
        h.scalar("SELECT CAST(attempt AS TEXT) FROM jobs").await,
        "2"
    );
    assert_eq!(h.sink.calls.lock().unwrap().len(), 2);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn followup_delegation_reuses_worker_placement_and_backend_session() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.runtime.pass().await.unwrap();
    let worker = h.scalar("SELECT id FROM workers").await;
    h.parent
        .responses
        .lock()
        .unwrap()
        .push_back(json!({"delegations":[{"worker_id":worker,"brief":"Run the followup checks"}]}));
    h.runtime
        .intake(Message {
            files: vec![],
            event_id: "e2".into(),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: "201.1".into(),
            thread_ts: Some("100.1".into()),
            sender: "UALICE".into(),
            text: "<@UOWNER> follow up".into(),
            source: "socket".into(),
            meta: None,
            attachments: vec![],
        })
        .await
        .unwrap();
    assert_eq!(h.runtime.pass().await.unwrap().started, 1);
    h.finish(1, 2).await;
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM workers").await,
        "1"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM obligations WHERE state='answered'")
            .await,
        "2"
    );
    assert_eq!(h.worker.calls.lock().unwrap()[1].resume, "backend-1");
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn delegation_validation_rejects_foreign_workers_ungranted_fetch_and_privileged_fields() {
    for request in [
        json!({"worker_id":"foreign-worker","brief":"run"}),
        json!({"fetch_repo":"owner/ungranted","fetch_ref":"main","brief":"run"}),
        json!({"fetch_ref":"main","brief":"run"}),
        json!({"clearance":"executor","brief":"run"}),
        json!({"role":"invented","brief":"run"}),
    ] {
        let h=Harness::new(vec![json!({"reply":{"text":"Not committed","status":"complete"},"delegations":[request]}),json!({})],false).await;
        h.store.call(|c|{
            c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated) VALUES('other','TTEAM','CROOM','99.1',1,1)",[])?;
            c.execute("INSERT INTO workers(id,session_id,machine,workspace,backend,created,updated) VALUES('foreign-worker','other','local','project','codex',1,1)",[])?;Ok(())
        }).await.unwrap();
        h.intake(false).await;
        h.runtime.pass().await.unwrap();
        assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
        assert_eq!(h.parent.calls.lock().unwrap()[1].call, "repair");
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
            "0"
        );
        assert!(h.sink.calls.lock().unwrap().is_empty());
        h.runtime.close().await.unwrap();
    }
}

struct CollectedFiles;
impl JobIo for CollectedFiles {
    fn prepare(&self, _: WorkerSpec, _: Job) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async { Ok(String::new()) })
    }
    fn collect(
        &self,
        _: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(async move {
            Ok(artifacts
                .into_iter()
                .enumerate()
                .map(|(i, reference)| CollectedArtifact {
                    reference,
                    data: (i == 0).then(|| b"verified artifact bytes".to_vec()),
                    error: if i == 0 { "" } else { "artifact_not_found" }.into(),
                })
                .collect())
        })
    }
}
#[tokio::test]
async fn ready_worker_artifacts_are_ordered_after_report_and_linked_to_answer_delivery() {
    let h = Harness::with_io(vec![delegate()], false, Arc::new(CollectedFiles)).await;
    h.worker.outcomes.lock().unwrap().push_back(Ok(Outcome{result:serde_json::from_value(json!({"status":"done","summary":"done","report":"Artifact ready.","artifacts":[{"path":"output/checks.md","kind":"md"},{"path":"missing.md","kind":"md"}]})).unwrap(),backend_session_id:"b".into()}));
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.sink.outcomes.lock().unwrap().extend([
        DeliveryOutcome::Sent {
            reference: "200.2".into(),
        },
        DeliveryOutcome::Ambiguous {
            code: "upload_timeout".into(),
        },
    ]);
    h.runtime.pass().await.unwrap();
    {
        let calls = h.sink.calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2].post.filename, "checks.md");
        assert_eq!(
            calls[2].post.blob.as_deref(),
            Some(b"verified artifact bytes".as_slice())
        );
        assert_eq!(calls[2].post.after, calls[1].post.idem_key);
    }
    assert_eq!(
        h.scalar("SELECT state FROM obligations").await,
        "awaiting_delivery"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM artifacts")
            .await,
        "2"
    );
    h.runtime.close().await.unwrap();
}
