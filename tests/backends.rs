use fridica::{
    core::{delivery::AdapterFuture, time::SequenceIds, worker::*},
    exec::{local::LocalTransport, process::Launch},
    workers::{
        jsonl::{JsonlWorker, Launcher, Options, WireRecorder},
        protocol::*,
    },
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
struct FakeLaunch {
    home: PathBuf,
    root: PathBuf,
    env: BTreeMap<String, String>,
}
impl Launcher for FakeLaunch {
    fn launch(&self, spec: &WorkerSpec, mut command: Vec<String>) -> Result<Launch, WorkerFailure> {
        let script = self.root.join(format!("{}.py", command[0]));
        command[0] = script.to_str().unwrap().into();
        command.insert(0, "python3".into());
        let mut env = self.env.clone();
        env.insert(
            "WORKER_LOG".into(),
            self.root.join("worker.log").to_str().unwrap().into(),
        );
        env.insert("SLACK_USER_TOKEN".into(), "xoxp-test-secret".into());
        Ok(LocalTransport {
            machine: spec.machine.clone(),
            home: self.home.clone(),
            excluded_env: spec.excluded_env.clone(),
        }
        .launch(
            command,
            &spec.workspace.path,
            std::env::vars_os(),
            &env,
            None,
            spec.create_cwd(),
        )
        .unwrap())
    }
}
#[derive(Default)]
struct Wire(Mutex<Vec<Value>>);
impl WireRecorder for Wire {
    fn record(&self, context: Value, event: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(json!({"context":context,"event":event}));
            Ok(())
        })
    }
}
struct Gate {
    decision: ApprovalDecision,
    requests: Mutex<Vec<ApprovalRequest>>,
    never: bool,
}
impl Gate {
    fn new(decision: ApprovalDecision) -> Self {
        Self {
            decision,
            requests: Mutex::new(vec![]),
            never: false,
        }
    }
}
impl ApprovalHandler for Gate {
    fn request(
        &self,
        _: WorkerRecord,
        _: Job,
        request: ApprovalRequest,
    ) -> AdapterFuture<'_, ApprovalDecision> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            if self.never {
                std::future::pending().await
            } else {
                self.decision
            }
        })
    }
}
struct Harness {
    directory: tempfile::TempDir,
    spec: WorkerSpec,
    worker: Arc<JsonlWorker>,
    wire: Arc<Wire>,
}
impl Harness {
    fn new(backend: &str, env: BTreeMap<String, String>, idle: f64) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir(root.join("work")).unwrap();
        std::fs::write(root.join("codex.py"), include_str!("corpus/fake_codex.py")).unwrap();
        std::fs::write(
            root.join("claude.py"),
            include_str!("corpus/fake_claude.py"),
        )
        .unwrap();
        let machine = json!({"name":"box","transport":"local","workspaces":[],"backends":["codex","claude"],"default_backend":backend,"policy":{},"host":"","tags":[],"resources":{"cpus":4},"max_workers":1,"max_jobs":1,"slurm":null,"description":""});
        let spec:WorkerSpec=serde_json::from_value(json!({"worker_id":"w1","machine":machine,"workspace":{"name":"work","path":root.join("work"),"policy":{"network":["github.com"]},"subfolders":false},"backend":backend,"instructions":"Speak as the owner.","model":"","reasoning_effort":"","job_timeout":3.0,"idle_timeout":idle,"excluded_env":["SLACK_USER_TOKEN"],"slot":1})).unwrap();
        let wire = Arc::new(Wire::default());
        let worker = Arc::new(
            JsonlWorker::new(
                spec.clone(),
                Arc::new(FakeLaunch {
                    home: root.into(),
                    root: root.into(),
                    env,
                }),
                Arc::new(SequenceIds::default()),
                Options {
                    // Long enough for a slow CI runner to reap a backend that
                    // exits shortly after closing its output.
                    eof_wait: Duration::from_secs(1),
                    close_grace: Duration::from_millis(100),
                },
                wire.clone(),
            )
            .unwrap(),
        );
        Self {
            directory,
            spec,
            worker,
            wire,
        }
    }
    fn logs(&self, kind: &str) -> Vec<Value> {
        std::fs::read_to_string(self.directory.path().join("worker.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["kind"] == kind)
            .collect()
    }
    async fn wait_log(&self, kind: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.logs(kind).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
    }
    async fn run(
        &self,
        prompt: &str,
        resume: &str,
        gate: Arc<dyn ApprovalHandler>,
    ) -> Result<Outcome, WorkerFailure> {
        run(self.worker.clone(), prompt.into(), resume.into(), gate).await
    }
}
async fn run(
    worker: Arc<JsonlWorker>,
    prompt: String,
    resume: String,
    gate: Arc<dyn ApprovalHandler>,
) -> Result<Outcome, WorkerFailure> {
    let record=serde_json::from_value(json!({"id":"w1","session_id":"thread","machine":"box","workspace":"work","backend":"codex"})).unwrap();
    let job = serde_json::from_value(
        json!({"id":"j1","worker_id":"w1","session_id":"thread","brief":prompt}),
    )
    .unwrap();
    worker
        .run(
            RunRequest {
                job_id: "j1".into(),
                attempt: 1,
                brief: prompt,
                resume,
                fork_from: String::new(),
            },
            record,
            job,
            gate,
        )
        .await
}
#[tokio::test]
async fn both_backends_return_results_reuse_processes_and_resume_after_close() {
    for backend in ["codex", "claude"] {
        let h = Harness::new(backend, BTreeMap::new(), 30.);
        let first = h
            .run("Build it", "", Arc::new(DenyApprovals))
            .await
            .unwrap();
        let second = h
            .run(
                "Test it",
                &first.backend_session_id,
                Arc::new(DenyApprovals),
            )
            .await
            .unwrap();
        assert_eq!(first.result.status, "done");
        assert_eq!(first.backend_session_id, second.backend_session_id);
        assert!(h.worker.alive());
        h.worker.close().await.unwrap();
        assert!(!h.worker.alive());
        let third = h
            .run("Again", &first.backend_session_id, Arc::new(DenyApprovals))
            .await
            .unwrap();
        assert_eq!(third.backend_session_id, first.backend_session_id);
        let kind = if backend == "codex" {
            "turn/start"
        } else {
            "user"
        };
        let logs = h.logs(kind);
        assert_eq!(logs[0]["pid"], logs[1]["pid"]);
        assert_ne!(logs[1]["pid"], logs[2]["pid"]);
        if backend == "codex" {
            let start = &h.logs("thread/start")[0];
            assert_eq!(start["data"]["params"]["approvalsReviewer"], "auto_review");
            assert_eq!(
                start["data"]["params"]["developerInstructions"],
                h.spec.instructions
            );
            assert_eq!(start["omp"], "4");
            assert!(start["slack"].is_null());
            assert_eq!(
                logs[0]["data"]["params"]["outputSchema"],
                fridica::workers::result::schema()
            );
        } else {
            assert!(logs[0]["argv"]
                .as_array()
                .unwrap()
                .contains(&json!("--strict-mcp-config")));
            assert!(uuid::Uuid::parse_str(&first.backend_session_id).is_ok());
        }
        assert!(h
            .wire
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["event"]["direction"] == "received"));
        h.worker.close().await.unwrap();
    }
}
#[tokio::test]
async fn missing_backend_sessions_restart_once_and_codex_prose_gets_a_summary() {
    for (backend, key) in [("codex", "LOST_THREAD"), ("claude", "LOST_SESSION")] {
        let h = Harness::new(
            backend,
            BTreeMap::from([(key.into(), "lost-session".into())]),
            30.,
        );
        let outcome = h
            .run("hello", "lost-session", Arc::new(DenyApprovals))
            .await
            .unwrap();
        assert_ne!(outcome.backend_session_id, "lost-session");
        assert_eq!(
            h.wire
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|v| v["event"]["direction"] == "start")
                .count(),
            2
        );
        h.worker.close().await.unwrap();
    }
    let h = Harness::new("codex", BTreeMap::new(), 30.);
    let o = h.run("PROSE", "", Arc::new(DenyApprovals)).await.unwrap();
    assert_eq!(o.result.summary, "summarized after prose");
    assert_eq!(h.logs("turn/start").len(), 2);
    h.worker.close().await.unwrap();
}
#[tokio::test]
async fn approvals_use_backend_envelopes_and_session_grants_are_cached() {
    for backend in ["codex", "claude"] {
        let h = Harness::new(backend, BTreeMap::new(), 30.);
        let gate = Arc::new(Gate::new(ApprovalDecision::Session));
        let prompt = if backend == "codex" {
            "APPROVE:make"
        } else {
            "TOOL:make"
        };
        let a = h.run(prompt, "", gate.clone()).await.unwrap();
        h.run(prompt, &a.backend_session_id, gate.clone())
            .await
            .unwrap();
        assert_eq!(gate.requests.lock().unwrap().len(), 1);
        let answer = &h.logs("approval-answer")[0]["data"];
        if backend == "codex" {
            assert_eq!(answer["result"]["decision"], "acceptForSession");
        } else {
            assert_eq!(answer["response"]["response"]["behavior"], "allow");
        }
        h.worker.close().await.unwrap();
    }
}
#[tokio::test]
async fn interrupts_cancel_pending_approvals_and_close_does_not_wait_for_them() {
    let h = Harness::new("codex", BTreeMap::new(), 30.);
    let gate = Arc::new(Gate {
        never: true,
        ..Gate::new(ApprovalDecision::Deny)
    });
    let task = tokio::spawn(run(
        h.worker.clone(),
        "APPROVE:make".into(),
        "".into(),
        gate.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    h.worker.interrupt().await.unwrap();
    let error = task.await.unwrap().err().unwrap();
    assert_eq!(error.kind, Failure::Interrupted);
    assert_eq!(
        h.logs("approval-answer")[0]["data"]["result"]["decision"],
        "decline"
    );
    assert!(!h.worker.alive());
    let task = tokio::spawn(run(
        h.worker.clone(),
        "APPROVE:make".into(),
        "".into(),
        gate.clone(),
    ));
    tokio::time::timeout(Duration::from_secs(2), async {
        while gate.requests.lock().unwrap().len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), h.worker.close())
        .await
        .unwrap()
        .unwrap();
    assert!(task.await.unwrap().is_err());
    assert!(!h.worker.alive());
}
#[tokio::test]
async fn interruption_caller_cancellation_and_idle_expiry_release_processes() {
    for backend in ["codex", "claude"] {
        let h = Harness::new(backend, BTreeMap::new(), 0.1);
        let kind = if backend == "codex" {
            "turn/start"
        } else {
            "user"
        };
        let task = tokio::spawn(run(
            h.worker.clone(),
            "HANG".into(),
            "".into(),
            Arc::new(DenyApprovals),
        ));
        h.wait_log(kind).await;
        h.worker.interrupt().await.unwrap();
        assert_eq!(
            task.await.unwrap().err().unwrap().kind,
            Failure::Interrupted
        );
        let task = tokio::spawn(run(
            h.worker.clone(),
            "HANG".into(),
            "".into(),
            Arc::new(DenyApprovals),
        ));
        tokio::time::timeout(Duration::from_secs(3), async {
            while h.logs(kind).len() < 2 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while h.worker.busy() || h.worker.alive() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        h.run("go", "", Arc::new(DenyApprovals)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert!(!h.worker.alive());
        h.worker.close().await.unwrap();
    }
}
#[tokio::test]
async fn backend_exits_and_refusals_have_truthful_codes_without_private_text() {
    let h = Harness::new("codex", BTreeMap::new(), 30.);
    let error = h
        .run("EXIT", "", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "backend_exit_3");
    assert!(!h.worker.alive());
    let error = h
        .run("FAIL", "", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, Failure::Refusal);
    assert_eq!(error.code, "backend_refused");
    h.worker.close().await.unwrap();
    let h = Harness::new("claude", BTreeMap::new(), 30.);
    let error = h
        .run("FAIL", "", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, "claude_job_failed");
    assert!(!error.code.contains("PRIVATE"));
    h.worker.close().await.unwrap();
}
/// A Claude turn stopped by the usage limit is a `RateLimited` failure with
/// the limit's reset time (#107, from fridica-agent); a later ordinary failure
/// on the same worker is not mistaken for one.
#[tokio::test]
async fn a_usage_limit_is_a_rate_limited_failure_with_its_reset_time() {
    let h = Harness::new("claude", BTreeMap::new(), 30.);
    let error = h
        .run("LIMIT", "", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(
        (error.kind, error.code.as_str()),
        (
            Failure::RateLimited {
                retry_at: Some(1790973000)
            },
            "claude_rate_limited"
        )
    );
    let error = h
        .run("FAIL", "", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(
        (error.kind, error.code.as_str()),
        (Failure::Execution, "claude_job_failed")
    );
    h.worker.close().await.unwrap();
}

impl Harness {
    fn rebuild(&mut self, env: BTreeMap<String, String>) {
        let root = self.directory.path();
        self.worker = Arc::new(
            JsonlWorker::new(
                self.spec.clone(),
                Arc::new(FakeLaunch {
                    home: root.into(),
                    root: root.into(),
                    env,
                }),
                Arc::new(SequenceIds::default()),
                Options {
                    eof_wait: Duration::from_millis(200),
                    close_grace: Duration::from_millis(100),
                },
                self.wire.clone(),
            )
            .unwrap(),
        );
    }
    fn script(&self, source: &str) {
        std::fs::write(
            self.directory
                .path()
                .join(format!("{}.py", self.spec.backend)),
            source,
        )
        .unwrap();
    }
}
const EARLY: &str = r#"import sys,json
emit=lambda x: (print(json.dumps(x),flush=True))
for line in sys.stdin:
    m=json.loads(line); method=m.get('method'); i=m.get('id')
    if method=='initialize': emit({'id':i,'result':{}})
    elif method=='thread/start': emit({'id':i,'result':{'thread':{'id':'early-thread'}}})
    elif method=='turn/start':
        body=json.dumps({'status':'done','summary':'early final','report':'final drained'})
        emit({'method':'item/completed','params':{'turnId':'t1','item':{'type':'agentMessage','text':body}}})
        emit({'method':'turn/completed','params':{'turn':{'id':'t1','status':'completed'}}})
        emit({'id':i,'result':{'turn':{'id':'t1'}}})
        sys.exit(0)
"#;
#[tokio::test]
async fn final_messages_before_start_ack_and_process_exit_are_not_lost() {
    let h = Harness::new("codex", BTreeMap::new(), 30.);
    h.script(EARLY);
    let result = h.run("go", "", Arc::new(DenyApprovals)).await.unwrap();
    assert_eq!(result.result.report, "final drained");
    h.worker.close().await.unwrap();
}
#[tokio::test]
async fn close_interrupts_a_blocked_stdin_write_without_waiting_for_job_deadline() {
    let mut h = Harness::new("codex", BTreeMap::new(), 30.);
    h.spec.job_timeout = 60.;
    h.rebuild(BTreeMap::new());
    h.script(
        r#"import sys,json,time
for line in sys.stdin:
 m=json.loads(line)
 if m.get('method')=='initialize': print(json.dumps({'id':m['id'],'result':{}}),flush=True)
 if m.get('method')=='thread/start':
  print(json.dumps({'id':m['id'],'result':{'thread':{'id':'blocked'}}}),flush=True)
  time.sleep(60)
"#,
    );
    let task = tokio::spawn(run(
        h.worker.clone(),
        "x".repeat(3 * 1024 * 1024),
        "".into(),
        Arc::new(DenyApprovals),
    ));
    tokio::time::timeout(Duration::from_secs(3), async {
        while !h
            .wire
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v["event"]["message"]["method"] == "turn/start")
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), h.worker.close())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(task.await.unwrap().err().unwrap().kind, Failure::Cancelled);
    assert!(!h.worker.alive());
}
#[tokio::test]
async fn malformed_output_and_delayed_eof_are_bounded_and_recorded() {
    for (script,code) in [
        ("import sys; sys.stdout.write('x'*(4*1024*1024+1)); sys.stdout.flush()", "backend_output_invalid_or_too_large"),
        ("import os,time,sys; os.close(1); time.sleep(.05); os.write(2,b'last diagnostic'); sys.exit(17)","backend_exit_17"),
        ("import os,time; os.close(1); time.sleep(60)","backend_output_closed_before_exit"),
    ] {
        let h=Harness::new("codex",BTreeMap::new(),30.);h.script(script);let error=h.run("go","",Arc::new(DenyApprovals)).await.err().unwrap();assert_eq!(error.code,code);assert!(!h.worker.alive());
        if code=="backend_exit_17"{let events=h.wire.0.lock().unwrap();let eof=events.iter().find(|v|v["event"]["direction"]=="eof").unwrap();assert_eq!(eof["event"]["status"],17);let bytes:Vec<u8>=serde_json::from_value(eof["event"]["stderr_tail"].clone()).unwrap();assert_eq!(bytes,b"last diagnostic");}
        h.worker.close().await.unwrap();
    }
}
#[tokio::test]
async fn missing_scoped_fetch_mcp_is_refused_before_thread_start_and_never_policy_denies() {
    let mut h = Harness::new("codex", BTreeMap::new(), 30.);
    h.spec.workspace.policy.fetch_repos = vec!["owner/repo".into()];
    h.rebuild(BTreeMap::from([("FAKE_MCP_CONFIG".into(), "1".into())]));
    assert_eq!(
        h.run("go", "", Arc::new(DenyApprovals))
            .await
            .err()
            .unwrap()
            .kind,
        Failure::Refusal
    );
    assert!(h.logs("thread/start").is_empty());
    h.worker.close().await.unwrap();
    let mut h = Harness::new("codex", BTreeMap::new(), 30.);
    h.spec.workspace.policy.approvals = "never".into();
    h.rebuild(BTreeMap::new());
    let gate = Arc::new(Gate::new(ApprovalDecision::Once));
    h.run("APPROVE:make", "", gate.clone()).await.unwrap();
    assert!(gate.requests.lock().unwrap().is_empty());
    assert_eq!(
        h.logs("approval-answer")[0]["data"]["result"]["decision"],
        "decline"
    );
    h.worker.close().await.unwrap();
}
#[tokio::test]
async fn approvals_timeout_to_denial_and_unrecognized_server_requests_are_declined() {
    let mut h = Harness::new("codex", BTreeMap::new(), 30.);
    h.spec.workspace.policy.approval_timeout = 0.02;
    h.rebuild(BTreeMap::new());
    let gate = Arc::new(Gate {
        never: true,
        ..Gate::new(ApprovalDecision::Once)
    });
    let o = h.run("APPROVE:make", "", gate).await.unwrap();
    assert_eq!(o.result.summary, "approval decline");
    h.run("ELICIT", &o.backend_session_id, Arc::new(DenyApprovals))
        .await
        .unwrap();
    assert_eq!(h.logs("elicit-answer")[0]["data"]["error"]["code"], -32601);
    h.worker.close().await.unwrap();
}
#[tokio::test]
async fn durable_supervisor_runs_real_protocol_adapters_and_records_wire_events() {
    use fridica::{
        approvals::Broker,
        config::{loader, registry::Registry, LoadContext},
        core::time::ReplayClock,
        core::Authority,
        store::{work, Store},
        workers::{
            instructions::OwnerInstructions,
            jsonl::{BackendFactory, StoreWireRecorder},
            supervisor::{Options as SupervisorOptions, Supervisor},
        },
    };
    for backend in ["codex", "claude"] {
        let h = Harness::new(backend, BTreeMap::new(), 30.);
        let root = h.directory.path();
        let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
        let mut config = loader::parse(
            corpus["source"].as_str().unwrap(),
            &root.join("config.toml"),
            &LoadContext {
                home: root.into(),
                runtime_dir: None,
                uid: users::get_current_uid(),
                protected: vec![],
            },
        )
        .unwrap();
        let mut machine = h.spec.machine.clone();
        machine.workspaces = vec![h.spec.workspace.clone()];
        config.machines = Registry {
            machines: vec![machine],
            default: "box".into(),
        };
        config.limits.job_timeout = 5.;
        config.limits.session_timeout = 10.;
        let store = Store::open(root.join("db")).await.unwrap();
        store.call(|c|{c.execute("INSERT INTO threads(id,workspace,channel,root_ts,created,updated) VALUES('thread','T','C','1',1,1)",[])?;Ok(())}).await.unwrap();
        let clock = Arc::new(ReplayClock::new(20.));
        let factory = Arc::new(BackendFactory {
            launcher: Arc::new(FakeLaunch {
                home: root.into(),
                root: root.into(),
                env: BTreeMap::new(),
            }),
            instructions: Arc::new(OwnerInstructions),
            ids: Arc::new(SequenceIds::default()),
            options: Options::default(),
            recorder: Arc::new(StoreWireRecorder {
                store: Arc::new(store.clone()),
                clock: clock.clone(),
            }),
        });
        let config = Arc::new(config);
        let (notifications, mut pending) = tokio::sync::mpsc::channel(1);
        let approvals = Arc::new(Broker::new(
            Arc::new(store.clone()),
            config.clone(),
            clock.clone(),
            Arc::new(SequenceIds::default()),
            Some(notifications),
        ));
        let supervisor = Supervisor::new(
            Arc::new(store.clone()),
            config,
            factory,
            approvals.clone(),
            Arc::new(NoJobIo),
            clock.clone(),
            SupervisorOptions::default(),
        )
        .unwrap();
        work::add_worker(&store,serde_json::from_value(json!({"id":"w1","session_id":"thread","machine":"box","workspace":"work","backend":backend})).unwrap(),1.).await.unwrap();
        work::enqueue(
            &store,
            serde_json::from_value(
                json!({"id":"j1","worker_id":"w1","session_id":"thread","brief":if backend=="codex" {"APPROVE:make"}else{"TOOL:make"}}),
            )
            .unwrap(),
            1.,
        )
        .await
        .unwrap();
        assert_eq!(supervisor.schedule().await.unwrap(), vec!["j1"]);
        let approval = tokio::time::timeout(Duration::from_secs(3), pending.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            work::get_worker(&store, "w1".into()).await.unwrap().status,
            "awaiting_approval"
        );
        assert!(approvals
            .decide(approval.clone(), ApprovalDecision::Once, Authority::Owner)
            .await
            .unwrap());
        assert_eq!(
            fridica::store::approvals::get(&store, approval)
                .await
                .unwrap()
                .unwrap()
                .status,
            "approved"
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while work::get_job(&store, "j1".into()).await.unwrap().status != "done" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        supervisor.settle().await.unwrap();
        let (inbox, wire): (i64, i64) = store
            .call(|c| {
                Ok((
                    c.query_row(
                        "SELECT COUNT(*) FROM thread_inbox WHERE kind='worker_result'",
                        [],
                        |r| r.get(0),
                    )?,
                    c.query_row(
                        "SELECT COUNT(*) FROM replay_events WHERE kind='backend_wire'",
                        [],
                        |r| r.get(0),
                    )?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(inbox, 1);
        assert!(wire >= 5);
        let previous = work::get_worker(&store, "w1".into())
            .await
            .unwrap()
            .backend_session_id;
        clock.set(40.);
        work::enqueue(&store,serde_json::from_value(json!({"id":"j2","worker_id":"w1","session_id":"thread","brief":"Fresh after expiry"})).unwrap(),40.).await.unwrap();
        assert_eq!(supervisor.schedule().await.unwrap(), vec!["j2"]);
        tokio::time::timeout(Duration::from_secs(5), async {
            while work::get_job(&store, "j2".into()).await.unwrap().status != "done" {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_ne!(
            work::get_worker(&store, "w1".into())
                .await
                .unwrap()
                .backend_session_id,
            previous
        );
        supervisor.close().await.unwrap();
    }
}

#[tokio::test]
async fn temporary_resume_errors_do_not_silently_start_a_new_session() {
    let h = Harness::new("codex", BTreeMap::new(), 30.);
    h.script(r#"import json,sys
for line in sys.stdin:
 m=json.loads(line)
 if m.get('method')=='initialize': print(json.dumps({'id':m['id'],'result':{}}),flush=True)
 elif m.get('method')=='thread/resume': print(json.dumps({'id':m['id'],'error':{'code':500,'message':'temporary server error'}}),flush=True)
 elif m.get('method')=='thread/start': raise AssertionError('must not start fresh')
"#);
    let error = h
        .run("go", "existing-session", Arc::new(DenyApprovals))
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind, Failure::Execution);
    assert_eq!(error.backend_session_id, "existing-session");
    assert_eq!(
        h.wire
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v["event"]["direction"] == "start")
            .count(),
        1
    );
    h.worker.close().await.unwrap();
}

#[tokio::test]
async fn claude_mode_fallback_remains_visible_without_failing_the_job() {
    let h = Harness::new(
        "claude",
        BTreeMap::from([("FAKE_PERMISSION_MODE".into(), "default".into())]),
        30.,
    );
    let result = h.run("go", "", Arc::new(DenyApprovals)).await.unwrap();
    assert_eq!(result.result.status, "done");
    assert!(h
        .wire
        .0
        .lock()
        .unwrap()
        .iter()
        .any(
            |v| v["event"]["notice"]["code"] == "claude_permission_mode_fallback"
                && v["event"]["notice"]["actual"] == "default"
        ));
    h.worker.close().await.unwrap();
}
