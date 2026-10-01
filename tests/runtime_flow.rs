#[path = "support/ordered_replay.rs"]
mod ordered_replay;
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Semaphore;
const SESSION: &str = "TTEAM:CROOM:100.1";
#[derive(Default)]
struct ParentScript {
    tape: Mutex<Option<Arc<ordered_replay::Tape>>>,
    calls: Mutex<Vec<ParentRequest>>,
    responses: Mutex<VecDeque<Value>>,
    gate: Mutex<Option<Arc<Semaphore>>>,
}
impl Parent for ParentScript {
    fn decide(&self, r: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            let tape = self.tape.lock().unwrap().clone();
            let call = tape.as_ref().map(|t| t.begin("parent.decide", json!(r)));
            self.calls.lock().unwrap().push(r);
            let gate = self.gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            let outcome = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or(ParentFailure {
                    code: "script_exhausted".into(),
                });
            if let Some(tape) = tape {
                serde_json::from_value(tape.finish(call.unwrap(), json!(outcome)).await).unwrap()
            } else {
                outcome
            }
        })
    }
}
#[derive(Default)]
struct Sink {
    tape: Mutex<Option<Arc<ordered_replay::Tape>>>,
    calls: Mutex<Vec<ClaimedPost>>,
    outcomes: Mutex<VecDeque<DeliveryOutcome>>,
}
impl Delivery for Sink {
    fn send(&self, p: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let tape = self.tape.lock().unwrap().clone();
            let call = tape.as_ref().map(|t| t.begin("delivery.send", json!(p)));
            let reference = format!("200.{}", p.id);
            self.calls.lock().unwrap().push(p);
            let outcome = self
                .outcomes
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(DeliveryOutcome::Sent { reference });
            if let Some(tape) = tape {
                serde_json::from_value(tape.finish(call.unwrap(), json!(outcome)).await).unwrap()
            } else {
                outcome
            }
        })
    }
}
struct WorkerScript {
    tape: Mutex<Option<Arc<ordered_replay::Tape>>>,
    calls: Mutex<Vec<RunRequest>>,
    outcomes: Mutex<VecDeque<Result<Outcome, WorkerFailure>>>,
    release: Semaphore,
    interrupt_wakes: AtomicBool,
    interruptions: AtomicUsize,
    close_fails: AtomicBool,
    instructions: Mutex<String>,
}
impl Default for WorkerScript {
    fn default() -> Self {
        Self {
            tape: Mutex::new(None),
            calls: Mutex::new(vec![]),
            outcomes: Mutex::new(VecDeque::new()),
            release: Semaphore::new(0),
            interrupt_wakes: AtomicBool::new(false),
            interruptions: AtomicUsize::new(0),
            close_fails: AtomicBool::new(false),
            instructions: Mutex::new("Scripted test instructions".into()),
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
        worker: WorkerRecord,
        job: Job,
        _: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>> {
        Box::pin(async move {
            let tape = self.script.tape.lock().unwrap().clone();
            let call = tape
                .as_ref()
                .map(|t| t.begin("worker.run", json!({"request":r,"worker":worker,"job":job})));
            self.script.calls.lock().unwrap().push(r);
            self.script.release.acquire().await.unwrap().forget();
            let outcome = self.script.outcomes.lock().unwrap().pop_front().unwrap_or_else(||Ok(Outcome{result:serde_json::from_value(json!({"status":"done","summary":"Checks passed","report":"Checks passed."})).unwrap(),backend_session_id:"backend-1".into()}));
            if let Some(tape) = tape {
                serde_json::from_value(tape.finish(call.unwrap(), json!(outcome)).await).unwrap()
            } else {
                outcome
            }
        })
    }
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async {
            self.script.interruptions.fetch_add(1, Ordering::SeqCst);
            if self.script.interrupt_wakes.load(Ordering::SeqCst) {
                self.script.release.add_permits(1);
            }
            Ok(())
        })
    }
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async {
            if self.script.close_fails.load(Ordering::SeqCst) {
                return Err(WorkerFailure {
                    kind: Failure::Execution,
                    code: "close_failed".into(),
                    backend_session_id: String::new(),
                });
            }
            self.alive.store(false, Ordering::SeqCst);
            Ok(())
        })
    }
}
struct Fakes(Arc<WorkerScript>);
impl Factory for Fakes {
    fn instructions(&self, _: &Config, _: &WorkerRecord) -> anyhow::Result<String> {
        Ok(self.0.instructions.lock().unwrap().clone())
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
    runtime: Arc<Runtime<ParentScript, Sink>>,
    ids: Arc<SequenceIds>,
}
impl Harness {
    async fn new(responses: Vec<Value>, observe: bool) -> Self {
        Self::with_io(responses, observe, Arc::new(NoJobIo)).await
    }
    async fn with_io(responses: Vec<Value>, observe: bool, job_io: Arc<dyn JobIo>) -> Self {
        Self::with_machines(responses, observe, job_io, "", None).await
    }
    async fn with_machines(
        responses: Vec<Value>,
        observe: bool,
        job_io: Arc<dyn JobIo>,
        machines: &str,
        machine_load: Option<Arc<fridica::machines::probe::Monitor>>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let source = format!(
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
{}
[state]
path="{}"
"#,
            dir.path().join("project").display(),
            machines,
            dir.path().join("db").display()
        );
        std::fs::write(dir.path().join("config.toml"), &source).unwrap();
        let config = loader::parse(
            &source,
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
        let ids = Arc::new(SequenceIds::default());
        let runtime = Runtime::start(
            store.clone(),
            config.clone(),
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(Fakes(worker.clone())),
                job_io,
                machine_load,
            },
            clock.clone(),
            ids.clone(),
            observe,
        )
        .await
        .unwrap()
        .with_configuration_editor(
            {
                let parent = parent.clone();
                Arc::new(move |_| Ok(parent.clone()))
            },
            LoadContext {
                home: dir.path().into(),
                runtime_dir: None,
                uid: 1,
                protected: vec![],
            },
        );
        Self {
            dir,
            store,
            config,
            clock,
            parent,
            worker,
            sink,
            runtime: Arc::new(runtime),
            ids,
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
struct Readings;
impl fridica::machines::probe::Reader for Readings {
    fn read<'a>(
        &'a self,
        machine: &'a fridica::config::registry::Machine,
        _: Duration,
    ) -> AdapterFuture<'a, Option<fridica::machines::probe::Reading>> {
        let output: &[u8] = match machine.name.as_str() {
            "gpu_a" => b"load 1 8\ngpu 0, 99, 100, 1000\n",
            "gpu_b" => b"load 1 8\ngpu 0, 3, 100, 1000\n",
            _ => b"",
        };
        Box::pin(async move { fridica::machines::probe::parse(output, 0.) })
    }
}

#[tokio::test]
async fn probed_load_reaches_the_parent_and_steers_tag_based_placement() {
    let gpus = r#"
[machines.gpu_a]
host="gpu-a"
tags=["cuda"]
backends=["codex"]
resources={cpus=8, gpus=[0]}
[machines.gpu_a.workspaces]
shared="/work/shared"
[machines.gpu_b]
host="gpu-b"
tags=["cuda"]
backends=["codex"]
resources={cpus=8, gpus=[0]}
[machines.gpu_b.workspaces]
shared="/work/shared"
"#;
    let decision = json!({"reply":{"text":"Training.","status":"complete"},
        "delegations":[{"brief":"Train the model","tags":["cuda"],"workspace":"shared"}]});
    let monitor = Arc::new(fridica::machines::probe::Monitor::new(Arc::new(Readings)));
    let h = Harness::with_machines(
        vec![decision],
        false,
        Arc::new(NoJobIo),
        gpus,
        Some(monitor),
    )
    .await;
    h.intake(false).await;
    let first = h.runtime.pass().await.unwrap();
    assert_eq!(first.started, 1);
    // gpu_a is first in configuration order but its only GPU is busy.
    assert_eq!(h.scalar("SELECT machine FROM workers").await, "gpu_b");
    let request = h.parent.calls.lock().unwrap()[0].clone();
    let load = &request.session["work"]["load"];
    assert_eq!(
        (
            load["gpu_a"]["saturated"].clone(),
            load["gpu_b"]["saturated"].clone()
        ),
        (json!(true), json!(false))
    );
    assert!(
        load.get("local").is_none(),
        "an unreadable machine is omitted, not guessed"
    );
    let machines = request.session["machines"].as_array().unwrap();
    assert!(machines
        .iter()
        .any(|m| m["name"] == "gpu_b" && m["load"]["score"] == json!(0.125)));
    assert_eq!(
        h.scalar(
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='machine_load_result'"
        )
        .await,
        "3"
    );
    h.finish(1, 1).await;
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
async fn delegation_errors_tell_the_parent_how_to_repair() {
    // An invented worker ID and a scoped fetch of an ungranted repository by URL.
    let mut stale = delegate();
    stale["delegations"][0]["worker_id"] = json!("w236-plan");
    let mut fetch = delegate();
    fetch["delegations"][0]["fetch_repo"] = json!("https://github.com/chengcli/snapy");
    fetch["delegations"][0]["fetch_ref"] = json!("main");
    for (invalid, hint) in [
        (stale, "leave worker_id empty to start a new worker"),
        (
            fetch,
            "leave fetch_repo and fetch_ref empty so the worker clones",
        ),
    ] {
        let h = Harness::new(vec![invalid, delegate()], false).await;
        h.intake(false).await;
        h.runtime.pass().await.unwrap();
        let calls = h.parent.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].call, "repair");
        assert!(
            calls[1].errors.iter().any(|e| e.contains(hint)),
            "{:?}",
            calls[1].errors
        );
        // The repaired action (a new worker, no fetch) is then accepted.
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
            "1"
        );
        h.runtime.close().await.unwrap();
    }
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
                machine_load: None,
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
            ids: _,
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
                machine_load: None,
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
async fn changed_worker_instructions_start_a_fresh_backend_session() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.finish(1, 1).await;
    h.runtime.pass().await.unwrap();
    let worker = h.scalar("SELECT id FROM workers").await;
    let follow_up = |n: u32| {
        h.parent.responses.lock().unwrap().push_back(
            json!({"delegations":[{"worker_id":worker,"brief":"Run the followup checks"}]}),
        );
        Message {
            files: vec![],
            event_id: format!("e{n}"),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: format!("20{n}.1"),
            thread_ts: Some("100.1".into()),
            sender: "UALICE".into(),
            text: "<@UOWNER> follow up".into(),
            source: "socket".into(),
            meta: None,
            attachments: vec![],
        }
    };
    // Unchanged instructions resume; changed ones (e.g. an edited contract) do not.
    for (n, change, resume) in [(2, false, "backend-1"), (3, true, "")] {
        if change {
            *h.worker.instructions.lock().unwrap() = "Work on any repository".into();
        }
        h.runtime.intake(follow_up(n)).await.unwrap();
        assert_eq!(h.runtime.pass().await.unwrap().started, 1);
        h.finish(1, n as usize).await;
        h.runtime.pass().await.unwrap();
        assert_eq!(
            h.worker.calls.lock().unwrap()[n as usize - 1].resume,
            resume,
            "{n}"
        );
    }
    // The fresh session's ID is recorded and resumed while instructions hold.
    assert_eq!(
        h.scalar("SELECT backend_session_id FROM workers").await,
        "backend-1"
    );
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

async fn control_call(
    h: &Harness,
    method: &str,
    target: &str,
    body: Value,
    authority: Authority,
) -> fridica::control::Response {
    use fridica::control::{api::Api, Backend, Request};
    Api::new(h.runtime.clone())
        .request(
            Request {
                method: method.into(),
                target: target.into(),
                body,
            },
            authority,
        )
        .await
}
#[tokio::test]
async fn channel_instructions_reach_the_most_recent_thread_by_name_or_id() {
    let h = Harness::new(vec![], false).await;
    let route = |channel: &str| format!("/channels/{channel}/instruct");
    let body = |id: &str| json!({"text":"Approve the clone for this run","client_id":id});
    // No thread in the channel yet: nothing to attach an instruction to.
    let none = control_call(
        &h,
        "POST",
        &route("CROOM"),
        body("channel-0000"),
        Authority::Owner,
    )
    .await;
    assert_eq!(
        (none.status, none.body["error"].clone()),
        (404, json!("no_thread_in_channel"))
    );
    h.intake(false).await;
    let later = Message {
        files: vec![],
        event_id: "e2".into(),
        workspace: "TTEAM".into(),
        channel: "CROOM".into(),
        ts: "200.1".into(),
        thread_ts: None,
        sender: "UALICE".into(),
        text: "<@UOWNER> another question".into(),
        source: "socket".into(),
        meta: None,
        attachments: vec![],
    };
    assert!(h.runtime.intake(later).await.unwrap().is_some());
    // Startup records channel names from conversations.info.
    h.store
        .call(|c| {
            c.execute(
                "INSERT INTO meta VALUES('slack_channel_names','{\"CROOM\":\"ai-human-plume\"}')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    for (channel, id) in [
        ("CROOM", "channel-1111"),
        ("ai-human-plume", "channel-2222"),
    ] {
        let reply = control_call(&h, "POST", &route(channel), body(id), Authority::Owner).await;
        assert_eq!(reply.status, 200, "{channel}: {:?}", reply.body);
        assert_eq!(reply.body["thread"], "TTEAM:CROOM:200.1", "{channel}");
        assert_eq!(reply.body["name"], "#ai-human-plume:200.1", "{channel}");
        assert_eq!(reply.body["queued"], true);
    }
    // Views show the readable name, and it addresses the thread in controls.
    h.store
        .call(|c| {
            c.execute("INSERT INTO meta VALUES('slack_workspace_name','scix')", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let list = control_call(&h, "GET", "/threads", json!({}), Authority::Owner).await;
    let listed = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == "TTEAM:CROOM:200.1")
        .unwrap()
        .clone();
    assert_eq!(listed["name"], "#ai-human-plume:200.1");
    assert_eq!(
        (
            listed["key"]["workspace_name"].clone(),
            listed["key"]["channel_name"].clone()
        ),
        (json!("scix"), json!("ai-human-plume"))
    );
    for reference in ["ai-human-plume:200.1", "CROOM:200.1", "TTEAM:CROOM:200.1"] {
        let shown = control_call(
            &h,
            "GET",
            &format!("/threads/{reference}"),
            json!({}),
            Authority::Owner,
        )
        .await;
        assert_eq!(
            (shown.status, shown.body["session"]["id"].clone()),
            (200, json!("TTEAM:CROOM:200.1")),
            "{reference}"
        );
    }
    let unknown = control_call(
        &h,
        "GET",
        "/threads/general:200.1",
        json!({}),
        Authority::Owner,
    )
    .await;
    assert_eq!(
        (unknown.status, unknown.body["error"].clone()),
        (404, json!("unknown_thread"))
    );
    let named = control_call(
        &h,
        "POST",
        "/threads/ai-human-plume:200.1/instruct",
        body("thread-5555"),
        Authority::Owner,
    )
    .await;
    assert_eq!(named.status, 200, "{:?}", named.body);
    // The same client ID is idempotent through the channel route too.
    let retry = control_call(
        &h,
        "POST",
        &route("ai-human-plume"),
        body("channel-2222"),
        Authority::Owner,
    )
    .await;
    assert_eq!(retry.status, 200);
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='owner_instruction' AND session_id='TTEAM:CROOM:200.1'")
            .await,
        "3"
    );
    for (channel, authority, status, code) in [
        ("general", Authority::Owner, 404, "unknown_channel"),
        ("CUNLISTED", Authority::Owner, 404, "unknown_channel"),
        ("CROOM", Authority::Overseer, 403, "owner_required"),
    ] {
        let reply =
            control_call(&h, "POST", &route(channel), body("channel-3333"), authority).await;
        assert_eq!(
            (reply.status, reply.body["error"].clone()),
            (status, json!(code)),
            "{channel}"
        );
    }
    let extra = control_call(
        &h,
        "POST",
        &route("CROOM"),
        json!({"text":"x","client_id":"channel-4444","thread":"elsewhere"}),
        Authority::Owner,
    )
    .await;
    assert_eq!(extra.status, 400);
}

#[tokio::test]
async fn authenticated_controls_protect_owner_pause_and_deduplicate_instructions() {
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    let route = format!("/threads/{SESSION}/instruct");
    let body = json!({"text":"  Run the checks  ","client_id":"retry-1234","actor":"arbitrary"});
    let (first, second) = tokio::join!(
        control_call(&h, "POST", &route, body.clone(), Authority::Owner),
        control_call(&h, "POST", &route, body.clone(), Authority::Owner)
    );
    assert_eq!(first.status, 200);
    assert_eq!(first.body, second.body);
    let pause = format!("/threads/{SESSION}/pause");
    assert_eq!(
        control_call(
            &h,
            "POST",
            &pause,
            json!({"actor":"overseer"}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    for action in ["resume", "pause"] {
        assert_eq!(
            control_call(
                &h,
                "POST",
                &format!("/threads/{SESSION}/{action}"),
                json!({"actor":"UOWNER"}),
                Authority::Overseer
            )
            .await
            .status,
            403
        );
    }
    let retry = control_call(&h, "POST", &route, body.clone(), Authority::Owner).await;
    assert_eq!(retry.body, first.body);
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='owner_instruction'")
            .await,
        "1"
    );
    assert_eq!(h.scalar("SELECT json_extract(payload_json,'$.text') FROM thread_inbox WHERE kind='owner_instruction'").await,"Run the checks");
    assert_eq!(
        control_call(
            &h,
            "POST",
            &route,
            json!({"text":"different","client_id":"retry-1234"}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(
        control_call(&h, "POST", &route, body, Authority::DesktopReadOnly)
            .await
            .status,
        403
    );
    assert_eq!(
        control_call(&h, "POST", &pause, json!({"reason":42}), Authority::Owner)
            .await
            .status,
        400
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/resume"),
            json!({}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "active");
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='control_request' AND complete=0").await,"0");
}
#[tokio::test]
async fn control_views_notes_approvals_and_retry_use_durable_state() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.store.call(|c|{c.execute("INSERT INTO approvals(id,worker_id,job_id,session_id,kind,summary,created,expires_at) SELECT 'approve',worker_id,id,session_id,'exec','Run check',20,300 FROM jobs LIMIT 1",[])?;
        c.execute("INSERT INTO audit(time,actor,action,target,details_json) SELECT 20,'system','approval.requested','approve',json_object('attempt',attempt) FROM jobs LIMIT 1",[])?;
        c.execute("INSERT INTO outbox(idem_key,session_id,kind,channel,thread_ts,text,blob,state,created) VALUES('failed',?,'file','CROOM','100.1','Report',X'010203','ambiguous',20)",[SESSION])?;Ok(())}).await.unwrap();
    for path in [
        "/status",
        "/threads",
        "/attention/threads",
        "/workers",
        "/jobs",
        "/approvals",
        "/outbox",
        "/activity",
        "/obligations",
        "/config",
        "/machines",
    ] {
        let response = control_call(&h, "GET", path, json!({}), Authority::DesktopReadOnly).await;
        assert_eq!(response.status, 200, "{path}: {}", response.body);
    }
    let status = control_call(&h, "GET", "/status", json!({}), Authority::Owner)
        .await
        .body;
    assert_eq!(status["pending_approvals"], 1);
    assert_eq!(status["running_jobs"], 1);
    assert_eq!(status["problem_posts"], 1);
    let detail = control_call(
        &h,
        "GET",
        &format!("/threads/{SESSION}"),
        json!({}),
        Authority::Owner,
    )
    .await;
    assert_eq!(detail.status, 200, "{}", detail.body);
    assert_eq!(detail.body["workers"][0]["process"], "busy");
    assert_eq!(detail.body["session"]["control"], "active");
    assert_eq!(detail.body["session"]["key"]["channel"], "CROOM");
    assert!(detail.body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .all(|v| v.get("files").is_none() && v.get("attachments").is_none()));
    let post = detail.body["outbox"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["state"] == "ambiguous")
        .unwrap();
    assert_eq!(post["has_file"], true);
    assert!(post.get("blob").is_none());
    assert_eq!(post["created"], 20.);
    let retry = format!("/outbox/{}/retry", post["id"]);
    assert_eq!(
        control_call(&h, "POST", &retry, json!({}), Authority::Overseer)
            .await
            .status,
        403
    );
    assert_eq!(
        control_call(&h, "POST", &retry, json!({}), Authority::Owner)
            .await
            .status,
        200
    );
    assert_eq!(
        control_call(&h, "POST", &retry, json!({}), Authority::Owner)
            .await
            .status,
        409
    );
    let notes = format!("/threads/{SESSION}/notes");
    assert_eq!(
        control_call(
            &h,
            "POST",
            &notes,
            json!({"data":{"summary":"Use this"},"expected":0,"actor":"spoof"}),
            Authority::Owner
        )
        .await
        .body,
        json!({"revision":1})
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            &notes,
            json!({"data":{},"expected":0}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(h.scalar("SELECT actor FROM notes").await, "UOWNER");
    assert_eq!(
        control_call(
            &h,
            "POST",
            "/approvals/approve",
            json!({"decision":"once","actor":"spoof"}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            "/approvals/approve",
            json!({"decision":"deny"}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(h.scalar("SELECT decided_by FROM approvals").await, "UOWNER");
    h.runtime.close().await.unwrap();
}
#[tokio::test]
async fn control_recording_faults_prevent_effects_or_report_uncertain_commits() {
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    let route = format!("/threads/{SESSION}/instruct");
    let body = json!({"text":"Proceed","client_id":"fault-1234"});
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_control BEFORE INSERT ON replay_events WHEN NEW.kind='control_request' BEGIN SELECT RAISE(ABORT,'sensitive storage diagnostic'); END;")?;Ok(())}).await.unwrap();
    let failed = control_call(&h, "POST", &route, body.clone(), Authority::Owner).await;
    assert_eq!(failed.status, 500);
    assert_eq!(failed.body, json!({"error":"control_recording_failed"}));
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='owner_instruction'")
            .await,
        "0"
    );
    h.store.call(|c|{c.execute_batch("DROP TRIGGER fail_control; CREATE TRIGGER fail_control BEFORE INSERT ON replay_events WHEN NEW.kind='control_response' BEGIN SELECT RAISE(ABORT,'secret response diagnostic'); END;")?;Ok(())}).await.unwrap();
    let failed = control_call(&h, "POST", &route, body.clone(), Authority::Owner).await;
    assert_eq!(failed.status, 500);
    assert_eq!(failed.body, json!({"error":"control_outcome_unrecorded"}));
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='owner_instruction'")
            .await,
        "1"
    );
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='control_request' AND complete=0").await,"1");
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_control;")?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        control_call(&h, "POST", &route, body, Authority::Owner)
            .await
            .status,
        200
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='owner_instruction'")
            .await,
        "1"
    );
}
#[tokio::test]
async fn closing_thread_stops_active_workers_and_cancels_pending_approvals() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.store.call(|c|{c.execute("INSERT INTO approvals(id,worker_id,job_id,session_id,kind,summary,created) SELECT 'pending-close',worker_id,id,session_id,'exec','Check',20 FROM jobs LIMIT 1",[])?;Ok(())}).await.unwrap();
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/close"),
            json!({}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "closed");
    for authority in [Authority::Owner, Authority::Overseer] {
        assert_eq!(
            control_call(
                &h,
                "POST",
                &format!("/threads/{SESSION}/pause"),
                json!({}),
                authority
            )
            .await
            .status,
            409
        );
    }
    h.runtime.pass().await.unwrap();
    assert_eq!(h.scalar("SELECT status FROM workers").await, "stopped");
    assert_ne!(h.scalar("SELECT status FROM jobs").await, "running");
    assert_ne!(
        h.scalar("SELECT status FROM approvals WHERE id='pending-close'")
            .await,
        "pending"
    );
    assert!(h.runtime.processes().await.is_empty());
    assert_eq!(h.parent.calls.lock().unwrap().len(), 1);
    h.runtime.close().await.unwrap();
}
#[tokio::test]
async fn instructions_reject_observe_only_and_out_of_scope_threads() {
    let h = Harness::new(vec![], true).await;
    h.intake(false).await;
    let route = format!("/threads/{SESSION}/instruct");
    let body = json!({"text":"Proceed","client_id":"scope-1234"});
    assert_eq!(
        control_call(&h, "POST", &route, body.clone(), Authority::Owner)
            .await
            .status,
        409
    );
    assert_eq!(h.runtime.pass().await.unwrap().started, 0);
    assert!(h.parent.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    h.store
        .call(|c| {
            c.execute("UPDATE threads SET channel='COTHER'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        control_call(&h, "POST", &route, body, Authority::Owner)
            .await
            .status,
        409
    );
}

#[tokio::test]
async fn socket_owner_pause_remains_responsive_during_parent_and_fences_late_effects() {
    use fridica::control::{
        api::Api,
        client::Client,
        server::{Access, Options, Server},
    };
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    let gate = Arc::new(Semaphore::new(0));
    *h.parent.gate.lock().unwrap() = Some(gate.clone());
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(h.dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let server = Server::bind(
        &h.config,
        Arc::new(Api::new(h.runtime.clone())),
        Access::OwnerPeer,
        Options::default(),
    )
    .await
    .unwrap();
    let runtime = h.runtime.clone();
    let pass = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.parent.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let client = Client::new(&h.config.state.control_socket, None).unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        client.request(
            "POST",
            &format!("/threads/{SESSION}/pause"),
            Some(json!({})),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    gate.add_permits(1);
    pass.await.unwrap().unwrap();
    assert!(h.worker.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "0"
    );
    server.close().await.unwrap();
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn instruction_transaction_rolls_back_and_idempotency_survives_reopen() {
    use fridica::threads::controls;
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    h.runtime
        .control(
            SESSION.into(),
            Control::Pause {
                reason: "Owner pause".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_instruction BEFORE INSERT ON thread_inbox WHEN NEW.kind='owner_instruction' BEGIN SELECT RAISE(ABORT,'disk fault'); END;")?;Ok(())}).await.unwrap();
    let route = format!("/threads/{SESSION}/instruct");
    let body = json!({"text":"Continue","client_id":"persist-1234"});
    assert_eq!(
        control_call(&h, "POST", &route, body.clone(), Authority::Owner)
            .await
            .status,
        500
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='thread.instruct'")
            .await,
        "0"
    );
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_instruction;")?;
            Ok(())
        })
        .await
        .unwrap();
    let first = control_call(&h, "POST", &route, body, Authority::Owner).await;
    assert_eq!(first.status, 200);
    h.runtime
        .control(
            SESSION.into(),
            Control::Pause {
                reason: "Later pause".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    h.runtime.close().await.unwrap();
    let Harness {
        dir,
        runtime,
        store,
        ..
    } = h;
    drop(runtime);
    drop(store);
    let store = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Ok(s) = Store::open(dir.path().join("db")).await {
                break s;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let id = controls::instruct(
        &store,
        SESSION.into(),
        "Continue".into(),
        "persist-1234".into(),
        Authority::Owner,
        21.,
    )
    .await
    .unwrap();
    assert_eq!(json!(id), first.body["instruction_id"]);
    let control: String = store
        .call(|c| Ok(c.query_row("SELECT control FROM threads", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(control, "paused");
}

#[tokio::test]
async fn clean_and_restore_clear_local_inputs_without_replaying_or_losing_new_intake() {
    let h = Harness::new(
        vec![json!({"reply":{"text":"New request handled.","status":"complete"}})],
        false,
    )
    .await;
    h.intake(false).await;
    h.store.call(|c|{
        c.execute("UPDATE messages SET files_json='[\"old.diff\"]',attachments_json='[{\"id\":\"FOLD\",\"url\":\"https://files.slack.com/private-old\"}]'",[])?;
        c.execute("UPDATE threads SET summary='Old summary',decisions_json='[\"Old decision\"]',wait_streak=2,no_progress=1,turns=4,context_json='{\"repo\":\"owner/repo\"}'",[])?;
        c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated,summary) VALUES('other','TTEAM','CROOM','90.1',1,1,'Keep this')",[])?;
        c.execute("INSERT INTO messages(event_id,workspace,channel,root_ts,ts,sender,text,source,received_at) VALUES('other-event','TTEAM','CROOM','90.1','90.1','UALICE','Keep this too','socket',1)",[])?;Ok(())
    }).await.unwrap();
    let instruction = json!({"text":"Old instruction","client_id":"clean-1234"});
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/instruct"),
            instruction.clone(),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    let clean = format!("/threads/{SESSION}/clean");
    let restore = format!("/threads/{SESSION}/restore");
    for authority in [Authority::Overseer, Authority::DesktopReadOnly] {
        assert_eq!(
            control_call(
                &h,
                "POST",
                &clean,
                json!({"actor":"UOWNER"}),
                authority.clone()
            )
            .await
            .status,
            403
        );
        assert_eq!(
            control_call(&h, "POST", &restore, json!({"actor":"UOWNER"}), authority)
                .await
                .status,
            403
        );
    }
    assert_eq!(
        control_call(&h, "POST", &clean, json!({}), Authority::Owner)
            .await
            .status,
        200
    );
    assert_eq!(
        h.scalar("SELECT control FROM threads WHERE id='TTEAM:CROOM:100.1'")
            .await,
        "cleaned"
    );
    assert_eq!(
        h.scalar("SELECT text||files_json||attachments_json FROM messages WHERE event_id='e1'")
            .await,
        "[][]"
    );
    assert_eq!(
        h.scalar("SELECT text FROM messages WHERE event_id='other-event'")
            .await,
        "Keep this too"
    );
    assert_eq!(
        h.scalar("SELECT state FROM obligations").await,
        "owner_closed"
    );
    assert_eq!(h.scalar("SELECT json_extract(payload_json,'$.text') FROM thread_inbox WHERE kind='owner_instruction'").await,"");
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE session_id='TTEAM:CROOM:100.1' AND state IN ('pending','processing')").await,"0");
    assert_eq!(
        control_call(&h, "POST", &restore, json!({}), Authority::Owner)
            .await
            .status,
        200
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/instruct"),
            instruction,
            Authority::Owner
        )
        .await
        .status,
        409
    );
    h.runtime.pass().await.unwrap();
    assert!(h.parent.calls.lock().unwrap().is_empty());
    h.runtime
        .intake(Message {
            event_id: "fresh".into(),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: "101.1".into(),
            thread_ts: Some("100.1".into()),
            sender: "UALICE".into(),
            text: "<@UOWNER> anything new?".into(),
            files: vec![],
            attachments: vec![],
            source: "socket".into(),
            meta: None,
        })
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    let requests = h.parent.calls.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let old = requests[0]
        .history
        .iter()
        .find(|v| v["event_id"] == "e1")
        .unwrap();
    assert_eq!(old["text"], "");
    assert_eq!(old["files"], json!([]));
    assert_eq!(old["attachments"], json!([]));
    assert_eq!(requests[0].session["summary"], "");
    assert_eq!(requests[0].session["context"]["repo"], "owner/repo");
}

#[tokio::test]
async fn restore_preserves_history_and_counters_and_never_replays_latest_message() {
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    h.store.call(|c|{c.execute("UPDATE thread_inbox SET state='done'",[])?;c.execute("UPDATE threads SET turns=4,wait_streak=2,no_progress=1,reset_at=50,status='blocked',summary='Keep history'",[])?;Ok(())}).await.unwrap();
    for closed in [
        Control::Close,
        Control::Archive,
        Control::Pause {
            reason: "Owner pause".into(),
        },
    ] {
        h.runtime
            .control(SESSION.into(), closed, Authority::Owner)
            .await
            .unwrap();
        assert!(h
            .runtime
            .control(SESSION.into(), Control::Restore, Authority::Overseer)
            .await
            .is_err());
        assert_eq!(
            control_call(
                &h,
                "POST",
                &format!("/threads/{SESSION}/restore"),
                json!({}),
                Authority::Owner
            )
            .await
            .status,
            200
        );
        assert_eq!(h.scalar("SELECT printf('%d/%d/%d/%.0f/%s/%s',turns,wait_streak,no_progress,reset_at,status,summary) FROM threads").await,"4/2/1/50/blocked/Keep history");
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='pending'")
                .await,
            "0"
        );
    }
}

#[tokio::test]
async fn clean_is_atomic_and_fences_a_parent_already_in_flight() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_clean BEFORE UPDATE OF files_json ON messages BEGIN SELECT RAISE(ABORT,'write fault'); END;")?;Ok(())}).await.unwrap();
    let path = format!("/threads/{SESSION}/clean");
    assert_eq!(
        control_call(&h, "POST", &path, json!({}), Authority::Owner)
            .await
            .status,
        500
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "active");
    assert_eq!(h.scalar("SELECT state FROM obligations").await, "open");
    assert_ne!(h.scalar("SELECT text FROM messages").await, "");
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_clean;")?;
            Ok(())
        })
        .await
        .unwrap();
    let gate = Arc::new(Semaphore::new(0));
    *h.parent.gate.lock().unwrap() = Some(gate.clone());
    let runtime = h.runtime.clone();
    let pass = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.parent.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        control_call(&h, "POST", &path, json!({}), Authority::Owner)
            .await
            .status,
        200
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/restore"),
            json!({}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    gate.add_permits(1);
    pass.await.unwrap().unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='pending'")
            .await,
        "0"
    );
    assert!(h.sink.calls.lock().unwrap().is_empty());
    assert!(h.worker.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn restore_cannot_bypass_pending_worker_cleanup_even_after_status_is_stopped() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/clean"),
            json!({}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    let restore = format!("/threads/{SESSION}/restore");
    assert_eq!(
        control_call(&h, "POST", &restore, json!({}), Authority::Owner)
            .await
            .status,
        409
    );
    assert_eq!(
        control_call(&h, "POST", &restore, json!({}), Authority::Owner)
            .await
            .body,
        json!({"error":"thread_cleanup_pending"})
    );
    // Failure after the process has stopped must retain its durable stop intent.
    h.store.call(|c|{c.execute_batch("CREATE TRIGGER fail_stop_ack BEFORE INSERT ON replay_events WHEN NEW.kind='thread_worker_stopped' BEGIN SELECT RAISE(ABORT,'ack fault'); END;")?;Ok(())}).await.unwrap();
    assert!(h.runtime.pass().await.is_err());
    assert_eq!(h.scalar("SELECT status FROM workers").await, "stopped");
    assert!(h.runtime.processes().await.is_empty());
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='thread_worker_stop' AND complete=0").await,"1");
    assert_eq!(
        control_call(&h, "POST", &restore, json!({}), Authority::Owner)
            .await
            .status,
        409
    );
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_stop_ack;")?;
            Ok(())
        })
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='thread_worker_stop' AND complete=0").await,"0");
    assert_eq!(
        control_call(&h, "POST", &restore, json!({}), Authority::Owner)
            .await
            .status,
        200
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn frozen_python_clean_restore_transitions_match_declared_projection() {
    use fridica::threads::controls;
    let corpus: Value = serde_json::from_str(include_str!("corpus/thread_lifecycle.json")).unwrap();
    for case in corpus["cases"].as_array().unwrap() {
        let h = Harness::new(vec![], false).await;
        let seed = corpus["seed"].clone();
        h.store
            .call(move |c| {
                for command in seed.as_array().unwrap() {
                    let params = command[1].as_array().unwrap().iter().map(|v| match v {
                        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
                        Value::Number(n) if n.is_i64() => {
                            rusqlite::types::Value::Integer(n.as_i64().unwrap())
                        }
                        Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap()),
                        _ => panic!("invalid fixture value"),
                    });
                    c.execute(
                        command[0].as_str().unwrap(),
                        rusqlite::params_from_iter(params),
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        for step in case.as_array().unwrap() {
            let action = match step["action"].as_str().unwrap() {
                "clean" => Control::Clean,
                "restore" => Control::Restore,
                "close" => Control::Close,
                "archive" => Control::Archive,
                _ => unreachable!(),
            };
            controls::apply(&h.store, SESSION.into(), action, Authority::Owner, 30.)
                .await
                .unwrap();
            let actual:Value=h.store.call(|c|{
                let raw:String=c.query_row("SELECT json_object('control',control,'status',status,'summary',summary,'turns',turns,'wait_streak',wait_streak,'no_progress',no_progress,'reset_at',reset_at) FROM threads",[],|r|r.get(0))?;
                let decisions:String=c.query_row("SELECT decisions_json FROM threads",[],|r|r.get(0))?;
                let repo:String=c.query_row("SELECT json_extract(context_json,'$.repo') FROM threads",[],|r|r.get(0))?;
                let messages:Vec<String>=c.prepare("SELECT json_object('event_id',event_id,'text',text,'files_json',files_json,'attachments_json',attachments_json) FROM messages ORDER BY id")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
                let text:String=c.query_row("SELECT json_extract(payload_json,'$.text') FROM thread_inbox WHERE kind='owner_instruction'",[],|r|r.get(0))?;
                let count:i64=c.query_row("SELECT count(*) FROM thread_inbox WHERE kind='message'",[],|r|r.get(0))?;
                Ok(json!({"thread":serde_json::from_str::<Value>(&raw)?,"decisions":serde_json::from_str::<Value>(&decisions)?,"context_repo":repo,"messages":messages.iter().map(|s|serde_json::from_str::<Value>(s).unwrap()).collect::<Vec<_>>(),"instruction_text":text,"message_inbox_count":count}))
            }).await.unwrap();
            assert_eq!(actual, step["expected"], "{}", step["action"]);
        }
    }
}

#[tokio::test]
async fn worker_stop_intents_recover_after_runtime_restart_before_restore() {
    for control in [Control::Close, Control::Archive, Control::Clean] {
        let h = Harness::new(vec![], false).await;
        h.intake(false).await;
        h.store.call(|c|{
            c.execute("INSERT INTO workers(id,session_id,machine,workspace,backend,created,updated) VALUES('restart-worker',?,'local','project','codex',20,20)",[SESSION])?;
            c.execute("INSERT INTO jobs(id,worker_id,session_id,brief,queued_at) VALUES('restart-job','restart-worker',?,'Never execute',20)",[SESSION])?;Ok(())
        }).await.unwrap();
        h.runtime
            .control(SESSION.into(), control.clone(), Authority::Owner)
            .await
            .unwrap();
        h.runtime
            .control(SESSION.into(), control, Authority::Owner)
            .await
            .unwrap();
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='thread_worker_stop' AND complete=0").await,"1");
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
            ids: _,
        } = h;
        drop(runtime);
        drop(store);
        let store = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(s) = Store::open(dir.path().join("db")).await {
                    break s;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let runtime = Runtime::start(
            store.clone(),
            config.clone(),
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(Fakes(worker.clone())),
                job_io: Arc::new(NoJobIo),
                machine_load: None,
            },
            clock.clone(),
            Arc::new(SequenceIds::default()),
            false,
        )
        .await
        .unwrap();
        let h = Harness {
            dir,
            store,
            config,
            clock,
            parent,
            worker,
            sink,
            runtime: Arc::new(runtime),
            ids: Arc::new(SequenceIds::default()),
        };
        assert!(h
            .runtime
            .control(SESSION.into(), Control::Restore, Authority::Owner)
            .await
            .is_err());
        h.runtime.pass().await.unwrap();
        assert_eq!(h.scalar("SELECT status FROM jobs").await, "cancelled");
        assert_eq!(h.scalar("SELECT status FROM workers").await, "stopped");
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='thread_worker_stop' AND complete=0").await,"0");
        h.runtime
            .control(SESSION.into(), Control::Restore, Authority::Owner)
            .await
            .unwrap();
        assert!(h.worker.calls.lock().unwrap().is_empty());
        h.runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn cleaned_parent_failure_cannot_create_fresh_signal_work() {
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    let gate = Arc::new(Semaphore::new(0));
    *h.parent.gate.lock().unwrap() = Some(gate.clone());
    let runtime = h.runtime.clone();
    let pass = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.parent.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.runtime
        .control(SESSION.into(), Control::Clean, Authority::Owner)
        .await
        .unwrap();
    h.runtime
        .control(SESSION.into(), Control::Restore, Authority::Owner)
        .await
        .unwrap();
    gate.add_permits(1);
    pass.await.unwrap().unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM obligations WHERE state!='owner_closed'")
            .await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='pending'")
            .await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='inbox.failed'")
            .await,
        "0"
    );
}

#[tokio::test]
async fn historical_backfill_and_closure_require_owner_authority_and_preserve_owner_pause() {
    let h = Harness::new(vec![], false).await;
    h.intake(false).await;
    h.clock.set(200.);
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
    // Simulate the historical rows that predate v6 mention tracking.
    h.store
        .call(|c| {
            c.execute("DELETE FROM obligations", [])?;
            c.execute("UPDATE messages SET mentions_owner=0", [])?;
            Ok(())
        })
        .await
        .unwrap();
    let body = json!({"since":100.,"until":101.,"apply":true,"client_id":"backfill-1234"});
    for authority in [Authority::Overseer, Authority::DesktopReadOnly] {
        assert_eq!(
            control_call(&h, "POST", "/obligations/backfill", body.clone(), authority)
                .await
                .status,
            403
        );
    }
    let response = control_call(
        &h,
        "POST",
        "/obligations/backfill",
        body.clone(),
        Authority::Owner,
    )
    .await;
    assert_eq!(response.status, 200, "{:?}", response.body);
    assert_eq!(response.body["count"], 1);
    assert_eq!(
        control_call(&h, "POST", "/obligations/backfill", body, Authority::Owner)
            .await
            .body,
        response.body
    );
    let id = response.body["items"][0]["id"].as_str().unwrap();
    let route = format!("/obligations/{id}/close");
    assert_eq!(
        control_call(
            &h,
            "POST",
            &route,
            json!({"reason":"Already handled"}),
            Authority::Overseer
        )
        .await
        .status,
        403
    );
    assert_eq!(
        control_call(
            &h,
            "POST",
            &route,
            json!({"reason":"Already handled"}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    assert_eq!(
        h.scalar("SELECT state FROM obligations").await,
        "owner_closed"
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='obligation.disposition'")
            .await,
        "1"
    );
    assert!(h.parent.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn parent_memory_is_validated_merged_bounded_and_available_on_next_turn() {
    let mut action = delegate();
    action["context"] = json!({"machine":"not-configured","workspace":"project"});
    action["note"] = json!({"repo":"new-repo","blocker":"new blocker"});
    action["decisions"] = json!(["new decision"]);
    let mut valid = action.clone();
    valid["context"] =
        json!({"machine":"local","workspace":"project","repo":"owner/repo","branch":"feature"});
    // Legacy delegate spelling remains accepted; placement cannot discard repo/branch.
    valid["delegate"] = valid
        .as_object_mut()
        .unwrap()
        .remove("delegations")
        .unwrap();
    let h=Harness::new(vec![action,valid,json!({"reply":{"send":false,"text":"","status":"complete"},"note":{"next_step":"Review"}})],false).await;
    h.intake(false).await;
    let old: Vec<_> = (0..20).map(|n| format!("decision {n}")).collect();
    h.store.call(move|c| {c.execute("UPDATE threads SET decisions_json=?",[json!(old).to_string()])?;
        c.execute("INSERT INTO notes(session_id,revision,actor,data_json,created) VALUES(?,1,'owner',?,10)",rusqlite::params![SESSION,json!({"repo":"old","assignee":"Alice","custom":"retain"}).to_string()])?;Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(h.parent.calls.lock().unwrap()[1].call, "repair");
    assert_eq!(h.scalar("SELECT json_extract(context_json,'$.repo')||'/'||json_extract(context_json,'$.branch') FROM threads").await,"owner/repo/feature");
    assert_eq!(h.scalar("SELECT json_array_length(decisions_json)||':'||json_extract(decisions_json,'$[0]')||':'||json_extract(decisions_json,'$[19]') FROM threads").await,"20:decision 1:new decision");
    assert_eq!(
        h.scalar("SELECT data_json FROM notes ORDER BY revision DESC LIMIT 1")
            .await,
        json!({"repo":"new-repo","assignee":"Alice","custom":"retain","blocker":"new blocker"})
            .to_string()
    );
    h.runtime
        .instruct(
            SESSION.into(),
            "Update the next step".into(),
            "memory-update-1".into(),
            Authority::Owner,
        )
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    {
        let calls = h.parent.calls.lock().unwrap();
        assert_eq!(calls[2].session["notes"]["revision"], 2);
        assert_eq!(calls[2].session["notes"]["data"]["assignee"], "Alice");
        assert_eq!(calls[2].session["decisions"][19], "new decision");
    }
    assert_eq!(h.sink.calls.lock().unwrap().len(), 1);
    assert_eq!(h.scalar("SELECT json_extract(data_json,'$.next_step') FROM notes ORDER BY revision DESC LIMIT 1").await,"Review");
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn blocked_note_replaces_stale_assignments_and_unsent_answers_require_repair() {
    let h=Harness::new(vec![json!({"reply":{"text":"Blocked.","status":"blocked"},"note":{"blocker":"Need access"}})],false).await;
    h.intake(false).await;
    h.store.call(|c| {c.execute("INSERT INTO notes(session_id,revision,actor,data_json,created) VALUES(?,1,'owner',?,10)",rusqlite::params![SESSION,json!({"repo":"keep","assignee":"Alice","next_step":"Old","blocker":"Old"}).to_string()])?;Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT data_json FROM notes ORDER BY revision DESC LIMIT 1")
            .await,
        json!({"repo":"keep","assignee":"","next_step":"","blocker":"Need access"}).to_string()
    );
    let obligation = h.scalar("SELECT id FROM obligations").await;
    h.parent.responses.lock().unwrap().extend([json!({"reply":{"send":false,"text":"","status":"complete","answers":[obligation]},"note":{"repo":"must not commit"}}),json!({"note":{"next_step":"Ask owner"}})]);
    h.runtime
        .instruct(
            SESSION.into(),
            "Record next step".into(),
            "blocked-memory".into(),
            Authority::Owner,
        )
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.parent.calls.lock().unwrap()[2].call, "repair");
    assert_eq!(h.scalar("SELECT state FROM obligations").await, "open");
    assert_eq!(
        h.scalar(
            "SELECT json_extract(data_json,'$.repo') FROM notes ORDER BY revision DESC LIMIT 1"
        )
        .await,
        "keep"
    );
    assert_eq!(h.sink.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn owner_note_edit_fences_all_late_parent_effects() {
    let h=Harness::new(vec![json!({"reply":{"text":"Done","status":"complete"},"note":{"assignee":"Model"},"summary":"late","decisions":["late"]})],false).await;
    h.intake(false).await;
    let gate = Arc::new(Semaphore::new(0));
    *h.parent.gate.lock().unwrap() = Some(gate.clone());
    let runtime = h.runtime.clone();
    let pass = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.parent.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        control_call(
            &h,
            "POST",
            &format!("/threads/{SESSION}/notes"),
            json!({"expected":0,"data":{"assignee":"Owner choice"}}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    gate.add_permits(1);
    pass.await.unwrap().unwrap();
    assert!(h.sink.calls.lock().unwrap().is_empty());
    assert_eq!(h.scalar("SELECT state FROM thread_inbox").await, "pending");
    assert_eq!(
        h.scalar("SELECT decisions_json||summary FROM threads")
            .await,
        "[]"
    );
    assert_eq!(
        h.scalar(
            "SELECT json_extract(data_json,'$.assignee') FROM notes ORDER BY revision DESC LIMIT 1"
        )
        .await,
        "Owner choice"
    );
}

#[tokio::test]
async fn debrief_in_flight_is_fenced_by_owner_pause_or_clean() {
    for clean in [false, true] {
        let h = Harness::new(
            vec![
                json!({"reply":{"text":"Finished","status":"complete","discussion":"finished"}}),
                json!({"debrief":"Late closing text"}),
            ],
            false,
        )
        .await;
        h.intake(false).await;
        let gate = Arc::new(Semaphore::new(1));
        *h.parent.gate.lock().unwrap() = Some(gate.clone());
        let runtime = h.runtime.clone();
        let pass = tokio::spawn(async move { runtime.pass().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.parent.calls.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        h.runtime
            .control(
                SESSION.into(),
                if clean {
                    Control::Clean
                } else {
                    Control::Pause {
                        reason: "Hold".into(),
                    }
                },
                Authority::Owner,
            )
            .await
            .unwrap();
        gate.add_permits(1);
        pass.await.unwrap().unwrap();
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox WHERE kind='debrief_root'")
                .await,
            "0"
        );
        assert_eq!(
            h.scalar("SELECT CAST(debriefed_turn AS TEXT) FROM threads")
                .await,
            "0"
        );
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM reply_reservations WHERE state='reserved' AND outbox_id IS NULL").await,"0");
    }
}

fn control_actor(h: &Harness) -> fridica::threads::actor::Actor<ParentScript> {
    fridica::threads::actor::Actor {
        config: Some(h.config.clone()),
        store: h.store.clone(),
        parent: h.parent.clone(),
        clock: h.clock.clone(),
        ids: Arc::new(fridica::core::time::RandomIds),
        owner: "UOWNER".into(),
        limits: h.config.attention.clone(),
        observe_only: false,
        parent_timeout: Duration::from_secs(3),
        machine_load: None,
    }
}
fn worker_control(worker: &str, op: &str) -> Value {
    json!({"worker_control":[{"worker_id":worker,"op":op}]})
}
async fn instruction(h: &Harness, action: Value) {
    h.parent.responses.lock().unwrap().push_back(action);
    h.runtime
        .instruct(
            SESSION.into(),
            "Control worker".into(),
            format!("control-{}", h.parent.calls.lock().unwrap().len()),
            Authority::Owner,
        )
        .await
        .unwrap();
}
async fn idle_control_worker(h: &Harness) {
    work::add_worker(&h.store,serde_json::from_value(json!({"id":"owned","session_id":SESSION,"machine":"local","workspace":"project","backend":"codex"})).unwrap(),20.).await.unwrap();
}

#[tokio::test]
async fn parent_worker_controls_repair_foreign_duplicate_and_conflicting_requests() {
    for action in [
        worker_control("foreign", "stop"),
        json!({"worker_control":[{"worker_id":"owned","op":"interrupt"},{"worker_id":"owned","op":"stop"}]}),
        json!({"worker_control":[{"worker_id":"owned","op":"stop"}],"delegations":[{"worker_id":"owned","brief":"New work"}]}),
        worker_control("owned", "kill"),
    ] {
        let h = Harness::new(vec![action, json!({})], false).await;
        h.intake(false).await;
        idle_control_worker(&h).await;
        h.store.call(|c| {c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated) VALUES('other','TTEAM','CROOM','200',20,20)",[])?;Ok(())}).await.unwrap();
        work::add_worker(&h.store,serde_json::from_value(json!({"id":"foreign","session_id":"other","machine":"local","workspace":"project","backend":"codex"})).unwrap(),20.).await.unwrap();
        h.runtime.pass().await.unwrap();
        assert_eq!(h.parent.calls.lock().unwrap()[1].call, "repair");
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control'").await,"0");
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM workers WHERE status='idle'")
                .await,
            "2"
        );
    }
}

#[tokio::test]
async fn parent_stop_cancels_queued_work_and_approvals_and_recovers_a_lost_ack() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.worker.interrupt_wakes.store(true, Ordering::SeqCst);
    let worker = h.scalar("SELECT id FROM workers").await;
    work::enqueue(
        &h.store,
        serde_json::from_value(
            json!({"id":"queued","worker_id":worker,"session_id":SESSION,"brief":"later"}),
        )
        .unwrap(),
        20.,
    )
    .await
    .unwrap();
    h.store.call(|c| {c.execute("INSERT INTO approvals(id,worker_id,job_id,session_id,kind,summary,created) SELECT 'pending',worker_id,id,session_id,'command','check',20 FROM jobs WHERE status='running'",[])?;
        c.execute_batch("CREATE TRIGGER fail_parent_control_ack BEFORE INSERT ON replay_events WHEN NEW.kind='parent_worker_control_result' BEGIN SELECT RAISE(ABORT,'ack fault'); END;")?;Ok(())}).await.unwrap();
    instruction(&h, worker_control(&worker, "stop")).await;
    assert!(h.runtime.pass().await.is_err());
    assert_eq!(
        h.scalar("SELECT status FROM jobs WHERE id='queued'").await,
        "cancelled"
    );
    assert_eq!(
        h.scalar("SELECT status FROM approvals WHERE id='pending'")
            .await,
        "cancelled"
    );
    assert_eq!(h.scalar("SELECT status FROM workers").await, "stopped");
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control' AND complete=0").await,"1");
    assert!(h.runtime.processes().await.is_empty());
    assert_eq!(
        h.scalar("SELECT actor FROM audit WHERE action='worker.stop'")
            .await,
        "parent"
    );
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Deferred
    );
    h.runtime
        .control(
            SESSION.into(),
            Control::Pause {
                reason: "Keep paused".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    h.runtime.close().await.unwrap();
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER fail_parent_control_ack;")?;
            Ok(())
        })
        .await
        .unwrap();
    let runtime = Runtime::start(
        h.store.clone(),
        h.config.clone(),
        Adapters {
            parent: h.parent.clone(),
            delivery: h.sink.clone(),
            workers: Arc::new(Fakes(h.worker.clone())),
            job_io: Arc::new(NoJobIo),
            machine_load: None,
        },
        h.clock.clone(),
        Arc::new(fridica::core::time::RandomIds),
        false,
    )
    .await
    .unwrap();
    runtime.pass().await.unwrap();
    runtime.pass().await.unwrap();
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control_result'").await,"1");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='worker.stop'")
            .await,
        "1"
    );
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
    assert_eq!(h.worker.calls.lock().unwrap().len(), 1);
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn parent_control_intent_fault_rolls_back_worker_memory_and_post_effects() {
    let mut action = worker_control("owned", "stop");
    action["reply"] = json!({"text":"Stopping","status":"complete"});
    action["note"] = json!({"next_step":"Review"});
    let h = Harness::new(vec![action], false).await;
    h.intake(false).await;
    idle_control_worker(&h).await;
    work::enqueue(
        &h.store,
        serde_json::from_value(
            json!({"id":"queued","worker_id":"owned","session_id":SESSION,"brief":"later"}),
        )
        .unwrap(),
        20.,
    )
    .await
    .unwrap();
    h.store.call(|c| {c.execute_batch("CREATE TRIGGER fail_parent_control BEFORE INSERT ON replay_events WHEN NEW.kind='parent_worker_control' BEGIN SELECT RAISE(ABORT,'intent fault'); END;")?;Ok(())}).await.unwrap();
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Failed
    );
    assert_eq!(h.scalar("SELECT status FROM workers").await, "idle");
    assert_eq!(h.scalar("SELECT status FROM jobs").await, "queued");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM notes").await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE actor='parent'")
            .await,
        "0"
    );
}

#[tokio::test]
async fn parent_interrupt_blocks_retry_and_admission_until_its_outcome_is_reconciled() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    let worker = h.scalar("SELECT id FROM workers").await;
    let original = h.scalar("SELECT id FROM jobs").await;
    work::enqueue(
        &h.store,
        serde_json::from_value(
            json!({"id":"next","worker_id":worker,"session_id":SESSION,"brief":"Next"}),
        )
        .unwrap(),
        20.,
    )
    .await
    .unwrap();
    instruction(&h, worker_control(&worker, "interrupt")).await;
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Committed
    );
    instruction(&h, json!({})).await;
    assert!(fridica::attention::claim_due(&h.store, SESSION.into(), 20.)
        .await
        .unwrap()
        .is_none());
    h.worker
        .outcomes
        .lock()
        .unwrap()
        .push_back(Err(WorkerFailure {
            kind: Failure::Execution,
            code: "execution_failed".into(),
            backend_session_id: "reuse-me".into(),
        }));
    h.finish(1, 1).await;
    assert_eq!(
        h.scalar("SELECT status FROM jobs WHERE id!='next'").await,
        "interrupted"
    );
    assert_eq!(h.worker.interruptions.load(Ordering::SeqCst), 0);
    assert!(
        work::claim(&h.store, "next".into(), 1, h.config.clone(), 20.)
            .await
            .unwrap()
            .is_none()
    );
    h.parent.responses.lock().unwrap().push_back(json!({}));
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT status FROM jobs WHERE id='next'").await,
        "running"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='job.retry'")
            .await,
        "0"
    );
    // Replay a stale persisted interrupt while a different job owns the worker.
    h.store.call(move|c| {c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_worker_control',20,?,0)",[json!({"session":SESSION,"inbox":1,"worker":worker,"op":"interrupt","job":original,"attempt":1}).to_string()])?;Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT status FROM jobs WHERE id='next'").await,
        "running"
    );
    assert_eq!(h.worker.interruptions.load(Ordering::SeqCst), 0);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn parent_interrupt_signals_once_and_never_stops_the_reusable_worker_record() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    let worker = h.scalar("SELECT id FROM workers").await;
    instruction(&h, worker_control(&worker, "interrupt")).await;
    h.runtime.pass().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.worker.interruptions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.worker.interruptions.load(Ordering::SeqCst), 1);
    assert_eq!(h.parent.calls.lock().unwrap().len(), 2);
    h.finish(1, 1).await;
    h.parent.responses.lock().unwrap().push_back(json!({}));
    h.runtime.pass().await.unwrap();
    assert_eq!(h.scalar("SELECT status FROM workers").await, "idle");
    assert_eq!(h.scalar("SELECT status FROM jobs").await, "interrupted");
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control_result'").await,"1");
    assert_eq!(
        h.scalar("SELECT actor FROM audit WHERE action='worker.interrupt'")
            .await,
        "parent"
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_parent_stop_drain_retains_owned_process_until_reconciliation() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    let worker = h.scalar("SELECT id FROM workers").await;
    instruction(&h, worker_control(&worker, "stop")).await;
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Committed
    );
    let runtime = h.runtime.clone();
    let drain = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.worker.interruptions.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drain.abort();
    assert!(drain.await.unwrap_err().is_cancelled());
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control' AND complete=0").await,"1");
    assert_eq!(h.runtime.processes().await.len(), 1);
    h.worker.release.add_permits(1);
    h.parent.responses.lock().unwrap().push_back(json!({}));
    h.runtime.pass().await.unwrap();
    assert!(h.runtime.processes().await.is_empty());
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control' AND complete=0").await,"0");
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn parent_control_snapshot_is_fenced_by_admission_or_owner_pause_during_the_call() {
    for pause in [false, true] {
        let h = Harness::new(vec![worker_control("owned", "stop")], false).await;
        h.intake(false).await;
        idle_control_worker(&h).await;
        work::enqueue(
            &h.store,
            serde_json::from_value(
                json!({"id":"queued","worker_id":"owned","session_id":SESSION,"brief":"Later"}),
            )
            .unwrap(),
            20.,
        )
        .await
        .unwrap();
        let gate = Arc::new(Semaphore::new(0));
        *h.parent.gate.lock().unwrap() = Some(gate.clone());
        let actor = control_actor(&h);
        let step = tokio::spawn(async move { actor.step(SESSION.into()).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.parent.calls.lock().unwrap().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if pause {
            h.runtime
                .control(
                    SESSION.into(),
                    Control::Pause {
                        reason: "Hold".into(),
                    },
                    Authority::Owner,
                )
                .await
                .unwrap();
        } else {
            assert!(
                work::claim(&h.store, "queued".into(), 1, h.config.clone(), 20.)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        gate.add_permits(1);
        assert_eq!(
            step.await.unwrap().unwrap(),
            fridica::threads::actor::Step::Stale
        );
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control'").await,"0");
        assert_ne!(h.scalar("SELECT status FROM workers").await, "stopped");
    }
}

#[tokio::test]
async fn observe_only_and_owner_pauses_never_apply_parent_worker_controls() {
    for observe in [false, true] {
        let h = Harness::new(vec![worker_control("owned", "stop")], observe).await;
        h.intake(false).await;
        idle_control_worker(&h).await;
        if !observe {
            h.runtime
                .control(
                    SESSION.into(),
                    Control::Pause {
                        reason: "Hold".into(),
                    },
                    Authority::Owner,
                )
                .await
                .unwrap();
        }
        h.runtime.pass().await.unwrap();
        assert!(h.parent.calls.lock().unwrap().is_empty());
        assert_eq!(h.scalar("SELECT status FROM workers").await, "idle");
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control'").await,"0");
    }
}

#[tokio::test]
async fn stale_parent_interrupt_cannot_hit_a_new_attempt_of_the_same_job() {
    let h = Harness::new(vec![delegate()], false).await;
    h.worker
        .outcomes
        .lock()
        .unwrap()
        .push_back(Err(WorkerFailure {
            kind: Failure::Execution,
            code: "failed".into(),
            backend_session_id: "reuse".into(),
        }));
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.worker.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.scalar("SELECT status FROM jobs").await != "queued" {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar("SELECT CAST(attempt AS TEXT) FROM jobs").await,
        "2"
    );
    h.store.call(|c| {c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) SELECT 'parent_worker_control',20,json_object('session',session_id,'inbox',inbox_id,'worker',worker_id,'op','interrupt','job',id,'attempt',1),0 FROM jobs",[])?;Ok(())}).await.unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(h.scalar("SELECT status FROM jobs").await, "running");
    assert_eq!(h.scalar("SELECT json_extract(payload_json,'$.outcome') FROM replay_events WHERE kind='parent_worker_control_result'").await,"target_superseded");
    assert_eq!(h.worker.interruptions.load(Ordering::SeqCst), 0);
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn unconfirmed_parent_stop_cleanup_stays_pending_and_blocks_restore() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    h.worker.interrupt_wakes.store(true, Ordering::SeqCst);
    h.worker.close_fails.store(true, Ordering::SeqCst);
    let worker = h.scalar("SELECT id FROM workers").await;
    instruction(&h, worker_control(&worker, "stop")).await;
    assert!(h.runtime.pass().await.is_err());
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control_result'").await,"0");
    assert_eq!(h.runtime.processes().await.len(), 1);
    assert!(h
        .runtime
        .control(SESSION.into(), Control::Restore, Authority::Owner)
        .await
        .is_err());
    assert!(h.runtime.pass().await.is_err());
    h.worker.close_fails.store(false, Ordering::SeqCst);
    h.parent.responses.lock().unwrap().push_back(json!({}));
    h.runtime.pass().await.unwrap();
    assert!(h.runtime.processes().await.is_empty());
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control_result'").await,"1");
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE action='worker.stop'")
            .await,
        "1"
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn parent_stop_effects_roll_back_when_a_later_note_write_fails() {
    let mut action = worker_control("owned", "stop");
    action["note"] = json!({"next_step":"Review"});
    let h = Harness::new(vec![action], false).await;
    h.intake(false).await;
    idle_control_worker(&h).await;
    work::enqueue(
        &h.store,
        serde_json::from_value(
            json!({"id":"queued","worker_id":"owned","session_id":SESSION,"brief":"later"}),
        )
        .unwrap(),
        20.,
    )
    .await
    .unwrap();
    h.store.call(|c| {c.execute_batch("CREATE TRIGGER fail_notes_after_control BEFORE INSERT ON notes BEGIN SELECT RAISE(ABORT,'note fault'); END;")?;Ok(())}).await.unwrap();
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Failed
    );
    assert_eq!(h.scalar("SELECT status FROM workers").await, "idle");
    assert_eq!(h.scalar("SELECT status FROM jobs").await, "queued");
    assert_eq!(
        h.scalar(
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control'"
        )
        .await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE kind='worker_result'")
            .await,
        "0"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM audit WHERE actor='parent'")
            .await,
        "0"
    );
}

#[tokio::test]
async fn idle_parent_interrupt_is_visible_and_observe_only_defers_reconciliation() {
    let h = Harness::new(vec![worker_control("owned", "interrupt")], false).await;
    h.intake(false).await;
    idle_control_worker(&h).await;
    assert_eq!(
        control_actor(&h).step(SESSION.into()).await.unwrap(),
        fridica::threads::actor::Step::Committed
    );
    let route = format!("/threads/{SESSION}");
    let before = control_call(&h, "GET", &route, json!({}), Authority::Owner).await;
    assert_eq!(before.body["worker_controls"][0]["complete"], false);
    assert_eq!(
        before.body["worker_controls"][0]["request"]["job"],
        Value::Null
    );
    let observer = Runtime::start(
        h.store.clone(),
        h.config.clone(),
        Adapters {
            parent: h.parent.clone(),
            delivery: h.sink.clone(),
            workers: Arc::new(Fakes(h.worker.clone())),
            job_io: Arc::new(NoJobIo),
            machine_load: None,
        },
        h.clock.clone(),
        Arc::new(fridica::core::time::RandomIds),
        true,
    )
    .await
    .unwrap();
    observer.pass().await.unwrap();
    observer.close().await.unwrap();
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_worker_control_result'").await,"0");
    h.runtime.pass().await.unwrap();
    let after = control_call(&h, "GET", &route, json!({}), Authority::Owner).await;
    assert_eq!(after.body["worker_controls"][0]["complete"], true);
    assert_eq!(after.body["worker_controls"][0]["outcome"], "idle_noop");
    instruction(&h, json!({})).await;
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.parent.calls.lock().unwrap()[1].session["work"]["controls"][0]["outcome"],
        "idle_noop"
    );
    assert!(h.worker.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn owner_configuration_edits_are_durable_and_reject_unauthorized_or_invalid_changes() {
    let h = Harness::new(vec![], false).await;
    let fingerprint = h.config.fingerprint.clone();
    h.store.call(move |c| { c.execute("INSERT INTO runtime(id,pid,started_at,heartbeat_at,slack_status,observe_only,config_fingerprint) VALUES(1,123,1,2,'connected',0,?)", [fingerprint])?; Ok(()) }).await.unwrap();
    let original = std::fs::read_to_string(&h.config.path).unwrap();
    for authority in [Authority::Overseer, Authority::DesktopReadOnly] {
        assert_eq!(
            control_call(
                &h,
                "PATCH",
                "/config/limits",
                json!({"max_jobs":1}),
                authority
            )
            .await
            .status,
            403
        );
    }
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/limits",
            json!({"max_no_progress":1}),
            Authority::Owner
        )
        .await
        .status,
        400
    );
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/limits",
            json!({"max_jobs":0}),
            Authority::Owner
        )
        .await
        .status,
        400
    );
    assert_eq!(std::fs::read_to_string(&h.config.path).unwrap(), original);
    assert_eq!(
        h.scalar(
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_edit'"
        )
        .await,
        "0"
    );
    let response = control_call(
        &h,
        "PATCH",
        "/config/limits",
        json!({"max_jobs":1}),
        Authority::Owner,
    )
    .await;
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body["limits"]["max_jobs"], 1);
    assert_eq!(h.runtime.config().limits.max_jobs, 1);
    assert_eq!(
        control_call(&h, "GET", "/config", json!({}), Authority::DesktopReadOnly)
            .await
            .body["limits"]["max_jobs"],
        1
    );
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_edit' AND complete=0").await, "0");
    assert_eq!(h.scalar("SELECT json_extract(payload_json,'$.outcome') FROM replay_events WHERE kind='configuration_result'").await, "applied");
    assert_eq!(
        h.scalar("SELECT config_fingerprint FROM runtime").await,
        h.runtime.config().fingerprint
    );
    let fingerprint = h.runtime.config().fingerprint.clone();
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/limits",
            json!({"max_jobs":1}),
            Authority::Owner
        )
        .await
        .status,
        200
    );
    assert_eq!(h.runtime.config().fingerprint, fingerprint);
    assert_eq!(
        h.scalar(
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_edit'"
        )
        .await,
        "1"
    );
    std::fs::write(
        &h.config.path,
        format!(
            "{}\n# external change\n",
            std::fs::read_to_string(&h.config.path).unwrap()
        ),
    )
    .unwrap();
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/parent",
            json!({"model":"new"}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(h.runtime.config().parent.model, "");
}

#[tokio::test]
async fn lost_configuration_ack_rebuilds_actor_limits_before_the_next_turn() {
    use fridica::{config::editor::Prepared, store::configuration};
    let mut two = delegate();
    two["delegations"]
        .as_array_mut()
        .unwrap()
        .push(delegate()["delegations"][0].clone());
    let h = Harness::new(
        vec![
            two,
            json!({"reply":{"text":"Adjusted plan.","status":"complete"}}),
        ],
        false,
    )
    .await;
    let context = LoadContext {
        home: h.dir.path().into(),
        runtime_dir: None,
        uid: 1,
        protected: vec![],
    };
    // File replacement completed while its caller disappeared; no runtime update.
    let edit = Prepared::new(
        &h.config,
        "limits",
        &json!({"max_delegations_per_turn":1}),
        &context,
    )
    .unwrap();
    configuration::replace(&h.store, edit, 20.).await.unwrap();
    assert_eq!(h.runtime.config().limits.max_delegations_per_turn, 3);
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    assert_eq!(h.runtime.config().limits.max_delegations_per_turn, 1);
    assert!(configuration::pending(&h.store).await.unwrap().is_none());
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM jobs").await,
        "0"
    );
    assert!(
        h.parent
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|r| r.call == "repair"
                && r.errors.iter().any(|e| e.contains("too many delegations")))
    );
    h.runtime.close().await.unwrap();
}

#[tokio::test]
async fn configuration_waits_for_parent_turn_but_does_not_block_owner_pause() {
    let h = Harness::new(vec![delegate()], false).await;
    let gate = Arc::new(Semaphore::new(0));
    *h.parent.gate.lock().unwrap() = Some(gate.clone());
    h.intake(false).await;
    let runtime = h.runtime.clone();
    let pass = tokio::spawn(async move { runtime.pass().await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.parent.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let runtime = h.runtime.clone();
    let edit = tokio::spawn(async move {
        runtime
            .update_configuration("parent", json!({"model":"changed"}), Authority::Owner)
            .await
    });
    h.runtime
        .control(
            SESSION.into(),
            Control::Pause {
                reason: "owner review".into(),
            },
            Authority::Owner,
        )
        .await
        .unwrap();
    assert_eq!(h.runtime.config().parent.model, "");
    gate.add_permits(1);
    pass.await.unwrap().unwrap();
    edit.await.unwrap().unwrap();
    assert_eq!(h.runtime.config().parent.model, "changed");
    assert_eq!(h.scalar("SELECT control FROM threads").await, "paused");
    assert!(h.worker.calls.lock().unwrap().is_empty());
    assert!(h.sink.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn configuration_faults_leave_reconcilable_intent_and_never_acknowledge_early() {
    let h = Harness::new(vec![], false).await;
    let original = std::fs::read_to_string(&h.config.path).unwrap();
    h.store.call(|c| { c.execute_batch("CREATE TRIGGER reject_config_intent BEFORE INSERT ON replay_events WHEN NEW.kind='configuration_edit' BEGIN SELECT RAISE(ABORT,'fixture intent failure'); END;")?; Ok(()) }).await.unwrap();
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/parent",
            json!({"model":"first"}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(std::fs::read_to_string(&h.config.path).unwrap(), original);
    assert_eq!(h.runtime.config().parent.model, "");
    h.store.call(|c| { c.execute_batch("DROP TRIGGER reject_config_intent; CREATE TRIGGER reject_config_ack BEFORE INSERT ON replay_events WHEN NEW.kind='configuration_result' BEGIN SELECT RAISE(ABORT,'fixture acknowledgement failure'); END;")?; Ok(()) }).await.unwrap();
    assert_eq!(
        control_call(
            &h,
            "PATCH",
            "/config/parent",
            json!({"model":"first"}),
            Authority::Owner
        )
        .await
        .status,
        409
    );
    assert_eq!(h.runtime.config().parent.model, "first");
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_edit' AND complete=0").await, "1");
    assert!(h.runtime.pass().await.is_err());
    h.store
        .call(|c| {
            c.execute_batch("DROP TRIGGER reject_config_ack;")?;
            Ok(())
        })
        .await
        .unwrap();
    h.runtime.pass().await.unwrap();
    assert_eq!(
        h.scalar(
            "SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_result'"
        )
        .await,
        "1"
    );
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='configuration_edit' AND complete=0").await, "0");
    assert!(h.parent.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn configuration_changes_keep_running_jobs_and_observers_inert() {
    let h = Harness::new(vec![delegate()], false).await;
    h.intake(false).await;
    h.runtime.pass().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.worker.calls.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.runtime
        .update_configuration(
            "limits",
            json!({"max_jobs":1, "job_timeout":1, "worker_idle":1}),
            Authority::Owner,
        )
        .await
        .unwrap();
    assert_eq!(h.scalar("SELECT status FROM jobs").await, "running");
    assert_eq!(h.worker.interruptions.load(Ordering::SeqCst), 0);
    assert_eq!(h.runtime.config().limits.job_timeout, 1.);
    h.worker.release.add_permits(1);
    h.runtime.close().await.unwrap();
    let observer = Harness::new(vec![], true).await;
    observer.intake(false).await;
    observer
        .runtime
        .update_configuration("parent", json!({"model":"unused"}), Authority::Owner)
        .await
        .unwrap();
    observer.runtime.pass().await.unwrap();
    assert!(observer.parent.calls.lock().unwrap().is_empty());
    assert!(observer.worker.calls.lock().unwrap().is_empty());
    assert!(observer.sink.calls.lock().unwrap().is_empty());
}

async fn replay_checkpoint(h: &Harness, tape: &ordered_replay::Tape, name: &str) -> Value {
    json!({"checkpoint":name,"state":tape.normalize(ordered_replay::snapshot(&h.store).await)})
}

impl Harness {
    async fn restart(self) -> Self {
        self.runtime.close().await.unwrap();
        let Self {
            dir,
            store,
            config,
            clock,
            parent,
            worker,
            sink,
            runtime,
            ids,
        } = self;
        drop(runtime);
        drop(store);
        let store = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match Store::open(dir.path().join("db")).await {
                    Ok(store) => break store,
                    Err(_) => tokio::time::sleep(Duration::from_millis(1)).await,
                }
            }
        })
        .await
        .unwrap();
        let runtime = Runtime::start(
            store.clone(),
            config.clone(),
            Adapters {
                parent: parent.clone(),
                delivery: sink.clone(),
                workers: Arc::new(Fakes(worker.clone())),
                job_io: Arc::new(NoJobIo),
                machine_load: None,
            },
            clock.clone(),
            ids.clone(),
            false,
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
            runtime: Arc::new(runtime),
            ids,
        }
    }
}

#[derive(Default)]
struct ReplayJobIo {
    tape: Mutex<Option<Arc<ordered_replay::Tape>>>,
}
impl JobIo for ReplayJobIo {
    fn prepare(
        &self,
        spec: WorkerSpec,
        job: Job,
    ) -> AdapterFuture<'_, Result<String, WorkerFailure>> {
        Box::pin(async move {
            let tape = self.tape.lock().unwrap().clone().unwrap();
            let call = tape.begin("context.prepare", json!({"spec":spec,"job":job}));
            let snapshot: Result<String, WorkerFailure> =
                Ok("\nExternal context: synthetic head abc123; required checks passed.\n".into());
            serde_json::from_value(tape.finish(call, json!(snapshot)).await).unwrap()
        })
    }
    fn collect(
        &self,
        spec: WorkerSpec,
        artifacts: Vec<ArtifactRef>,
    ) -> AdapterFuture<'_, Result<Vec<CollectedArtifact>, WorkerFailure>> {
        Box::pin(async move {
            let tape = self.tape.lock().unwrap().clone().unwrap();
            let call = tape.begin(
                "context.artifacts",
                json!({"spec":spec,"artifacts":artifacts}),
            );
            let result = NoJobIo.collect(spec, artifacts).await;
            serde_json::from_value(tape.finish(call, json!(result)).await).unwrap()
        })
    }
}

async fn complete_runtime_capture(
    scenario: &str,
    rows: Option<Vec<ordered_replay::Event>>,
) -> Value {
    let replaying = rows.is_some();
    let responses = if replaying || scenario == "parent_error" {
        vec![]
    } else {
        vec![delegate()]
    };
    let io = Arc::new(ReplayJobIo::default());
    let mut h = Harness::with_io(responses, scenario == "observe", io.clone()).await;
    let tape = if let Some(rows) = rows {
        ordered_replay::Tape::replay(rows, h.clock.clone()).unwrap()
    } else {
        ordered_replay::Tape::record(h.clock.clone())
    };
    tape.bind(h.dir.path().to_str().unwrap(), &h.config.fingerprint);
    *io.tape.lock().unwrap() = Some(tape.clone());
    *h.parent.tape.lock().unwrap() = Some(tape.clone());
    *h.worker.tape.lock().unwrap() = Some(tape.clone());
    *h.sink.tape.lock().unwrap() = Some(tape.clone());
    if !replaying {
        if scenario == "ambiguous" {
            h.sink.outcomes.lock().unwrap().extend([
                DeliveryOutcome::Sent {
                    reference: "200.1".into(),
                },
                DeliveryOutcome::Ambiguous {
                    code: "connection_lost_after_write".into(),
                },
            ]);
        }
        if scenario == "rate_limit" {
            h.sink
                .outcomes
                .lock()
                .unwrap()
                .push_back(DeliveryOutcome::RateLimited { retry_after: 30. });
        }
        if scenario == "retry" {
            h.worker
                .outcomes
                .lock()
                .unwrap()
                .push_back(Err(WorkerFailure {
                    kind: Failure::Execution,
                    code: "remote_disconnected".into(),
                    backend_session_id: "same-session".into(),
                }));
        }
    }
    let mut snapshots = vec![];
    h.intake(false).await; // Includes duplicate intake with the same event identity.
    snapshots.push(replay_checkpoint(&h, &tape, "duplicate_intake").await);
    let first = h.runtime.pass().await.unwrap();
    if first.started > 0 {
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.worker.calls.lock().unwrap().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
    snapshots.push(replay_checkpoint(&h, &tape, "first_pass").await);
    if scenario == "owner_pause" {
        h.runtime
            .control(
                SESSION.into(),
                Control::Pause {
                    reason: "Owner requests a review".into(),
                },
                Authority::Owner,
            )
            .await
            .unwrap();
        snapshots.push(replay_checkpoint(&h, &tape, "owner_pause").await);
    }
    if scenario == "retry" {
        h.worker.release.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.scalar("SELECT status FROM jobs").await != "queued" {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        snapshots.push(replay_checkpoint(&h, &tape, "retry_queued").await);
        h.clock.set(21.);
        assert_eq!(h.runtime.pass().await.unwrap().started, 1);
    }
    if first.started > 0 {
        h.finish(1, 1).await;
        h.clock.set(22.);
        if scenario == "restart_result" || scenario == "restart_sending" {
            if scenario == "restart_sending" {
                let actor = fridica::threads::actor::Actor {
                    store: h.store.clone(),
                    config: Some(h.config.clone()),
                    parent: h.parent.clone(),
                    clock: h.clock.clone(),
                    ids: h.ids.clone(),
                    owner: "UOWNER".into(),
                    limits: h.config.attention.clone(),
                    observe_only: false,
                    parent_timeout: Duration::from_secs(1),
                    machine_load: None,
                };
                actor.step(SESSION.into()).await.unwrap();
                assert!(fridica::store::outbox::claim(&h.store, 22.)
                    .await
                    .unwrap()
                    .is_some());
            }
            snapshots.push(replay_checkpoint(&h, &tape, "before_restart").await);
            h = h.restart().await;
            snapshots.push(replay_checkpoint(&h, &tape, "after_restart").await);
        }
        h.runtime.pass().await.unwrap();
        snapshots.push(replay_checkpoint(&h, &tape, "completion").await);
        if scenario == "owner_pause" {
            assert!(h
                .runtime
                .control(SESSION.into(), Control::Resume, Authority::System)
                .await
                .is_err());
            h.runtime
                .control(SESSION.into(), Control::Resume, Authority::Owner)
                .await
                .unwrap();
            h.runtime.pass().await.unwrap();
            snapshots.push(replay_checkpoint(&h, &tape, "owner_resume").await);
        }
        if scenario == "rate_limit" {
            h.clock.set(51.);
            h.runtime.pass().await.unwrap();
            snapshots.push(replay_checkpoint(&h, &tape, "delivery_due").await);
        }
        h.runtime.pass().await.unwrap();
        snapshots.push(replay_checkpoint(&h, &tape, "no_duplicate_effects").await);
    }
    h.runtime.close().await.unwrap();
    snapshots.push(replay_checkpoint(&h, &tape, "closed").await);
    json!({"scenario":scenario,"events":tape.rows(),"snapshots":snapshots})
}

#[tokio::test]
async fn complete_ordered_runtime_tapes_replay_every_durable_column_without_exclusions() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus/runtime");
    for scenario in [
        "success",
        "ambiguous",
        "rate_limit",
        "retry",
        "owner_pause",
        "parent_error",
        "observe",
        "restart_result",
        "restart_sending",
    ] {
        let captured = complete_runtime_capture(scenario, None).await;
        let path = root.join(format!("{scenario}.json"));
        if std::env::var_os("FRIDICA_CAPTURE_REPLAY").is_some() {
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                &path,
                serde_json::to_string_pretty(&captured).unwrap() + "\n",
            )
            .unwrap();
        }
        let expected: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(captured, expected, "capture changed: {scenario}");
        let replayed = complete_runtime_capture(
            scenario,
            Some(serde_json::from_value(expected["events"].clone()).unwrap()),
        )
        .await;
        assert_eq!(replayed, expected, "replay differs: {scenario}");
    }
}

#[tokio::test]
async fn ordered_tape_rejects_incomplete_or_malformed_captures_and_enforces_completion_order() {
    let clock = Arc::new(ReplayClock::new(10.));
    let record = ordered_replay::Tape::record(clock.clone());
    let slow = record.begin("context.read", json!({"ref":"head"}));
    let fast = record.begin("worker.run", json!({"job":"one"}));
    record
        .finish(fast, json!({"Err":{"kind":"cancelled"}}))
        .await;
    clock.set(11.);
    record
        .finish(slow, json!({"Ok":{"context":"complete snapshot"}}))
        .await;
    let rows = record.rows();
    let replay = ordered_replay::Tape::replay(rows.clone(), clock).unwrap();
    let a = replay.begin("context.read", json!({"ref":"head"}));
    let b = replay.begin("worker.run", json!({"job":"one"}));
    // Poll the slow completion first; the tape must still deliver the fast one first.
    let (a, b) = tokio::join!(replay.finish(a, Value::Null), replay.finish(b, Value::Null));
    assert_eq!(a["Ok"]["context"], "complete snapshot");
    assert_eq!(b["Err"]["kind"], "cancelled");
    replay.rows();
    for n in [1, 2, 3] {
        assert!(ordered_replay::validate(&rows[..n]).is_err());
    }
    let mut bad = rows.clone();
    bad[0].seq = 9;
    assert!(ordered_replay::validate(&bad).is_err());
    let mut bad = rows.clone();
    bad[2].payload["id"] = json!(999);
    assert!(ordered_replay::validate(&bad).is_err());
    let mut bad = rows;
    bad[1].payload["id"] = json!(1);
    assert!(ordered_replay::validate(&bad).is_err());
}

#[tokio::test]
async fn restarted_throttling_and_concurrent_passes_preserve_owner_controls_and_deliver_once() {
    let mut h = Harness::new(vec![delegate(), json!({})], false).await;
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
    h = h.restart().await;
    assert_eq!(
        h.scalar("SELECT state FROM thread_inbox WHERE kind='worker_result'")
            .await,
        "pending"
    );
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
    let mut passes = vec![];
    for _ in 0..12 {
        let runtime = h.runtime.clone();
        passes.push(tokio::spawn(async move { runtime.pass().await.unwrap() }));
    }
    for pass in passes {
        pass.await.unwrap();
    }
    assert_eq!(h.worker.calls.lock().unwrap().len(), 1);
    assert_eq!(
        h.sink
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.post.kind == "report")
            .count(),
        1
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM reply_reservations WHERE state='reserved'")
            .await,
        "0"
    );
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
