use fridica::{
    config::{loader, Config, LoadContext},
    core::{
        delivery::*,
        time::{ReplayClock, SequenceIds},
        worker::ApprovalDecision,
        Authority,
    },
    daemon::composition::{self, ComposedRuntime, Execution, Host, Mode},
    exec::fetch::{FetchError, Fetched, Fetcher, Request},
    slack::{files, links},
    store::Store,
    threads::controls::Control,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const SESSION: &str = "TTEAM:CROOM:100.1";
const PARENT: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
prompt=sys.stdin.read()
assert 'SLACK_USER_TOKEN' not in os.environ
assert 'FRIDICA_GITHUB_TOKEN' not in os.environ
with pathlib.Path(os.environ['PARENT_LOG']).open('a') as log:
    log.write(json.dumps({'prompt':prompt,'argv':sys.argv})+'\n')
schema=json.loads(sys.argv[sys.argv.index('--json-schema')+1])
action={'decision':'respond'} if 'decision' in schema['properties'] else json.loads(os.environ['PARENT_ACTION'])
for code in ['worker_isolation_settings_refused', 'worker_mcp_settings_refused']:
    if code in prompt:
        action={'reply':{'text':'Worker refused: '+code,'status':'complete'}}
print(json.dumps({'is_error':False,'structured_output':action}))
"#;
const GH: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
assert sys.argv[1]=='api' and '/repos/o/r/issues/1' in sys.argv
assert os.environ['GH_TOKEN']=='ghp_owner_fixture'
pathlib.Path(os.environ['HOME']).joinpath('gh-called').write_text('called')
print('HTTP/2.0 200 OK\r\n\r\n'+json.dumps({'number':1,'title':'Synthetic issue','body':'context evidence','state':'open','user':{'login':'owner'}}))
"#;

#[derive(Default)]
struct Slack {
    sent: Mutex<Vec<ClaimedPost>>,
    downloads: AtomicUsize,
    links: AtomicUsize,
    throttle_upload: AtomicBool,
}
impl Delivery for Slack {
    fn send(&self, post: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            self.sent.lock().unwrap().push(post.clone());
            if post.post.kind == "upload" && self.throttle_upload.swap(false, Ordering::SeqCst) {
                DeliveryOutcome::RateLimited { retry_after: 1. }
            } else {
                DeliveryOutcome::Sent {
                    reference: format!("200.{}", post.id),
                }
            }
        })
    }
}
impl files::Downloader for Slack {
    fn download(
        &self,
        url: String,
        _: bool,
    ) -> AdapterFuture<'_, Result<files::Download, files::Failure>> {
        Box::pin(async move {
            assert_eq!(url, "https://files.slack.com/note");
            self.downloads.fetch_add(1, Ordering::SeqCst);
            Ok(files::Download {
                data: b"attachment evidence".to_vec(),
                size: 19,
            })
        })
    }
}
impl links::Reader for Slack {
    fn fetch(
        &self,
        _: links::Link,
    ) -> AdapterFuture<'_, Result<Vec<links::Entry>, links::Failure>> {
        Box::pin(async move {
            self.links.fetch_add(1, Ordering::SeqCst);
            Ok(vec![links::Entry {
                sender: "UALICE".into(),
                text: "linked evidence".into(),
                ts: "99.000002".into(),
            }])
        })
    }
}
#[derive(Default)]
struct Fetch(Mutex<Vec<Request>>);
impl Fetcher for Fetch {
    fn fetch(&self, request: Request) -> AdapterFuture<'_, Result<Fetched, FetchError>> {
        Box::pin(async move {
            let result = Fetched {
                path: request.path(),
                commit: "a".repeat(40),
            };
            self.0.lock().unwrap().push(request);
            Ok(result)
        })
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    config: Arc<Config>,
    store: Store,
    clock: Arc<ReplayClock>,
    slack: Arc<Slack>,
    fetch: Arc<Fetch>,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["bin", "home", "private", "project", "parent-temp"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        // Existing protocol fixture, with a deterministic session and a synthetic artifact.
        let worker=include_str!("corpus/fake_codex.py")
            .replace("\"thr_\" + str(os.getpid())", "\"fixture-thread\"")
            .replace("done(result(\"made a plot\",", "pathlib.Path(path).write_bytes(b'\\x89PNG\\r\\n\\x1a\\nfixture'); done(result(\"made a plot\",");
        for (name, text) in [("claude", PARENT), ("codex", worker.as_str()), ("gh", GH)] {
            let path = root.join("bin").join(name);
            std::fs::write(&path, text).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let source = r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[parent]
backend="claude"
timeout=10
[limits]
job_timeout=10
[machines.local]
backends=["codex"]
[machines.local.policy]
mode="write"
approvals="on-request"
fetch_repos=["o/r"]
gpu_confine=false
[machines.local.workspaces]
project="project"
[state]
path="private/db"
control_socket="private/control.sock"
[isolation]
mcp_aliases=["owner-fridica"]
"#;
        std::fs::write(root.join("config.toml"), source).unwrap();
        let config = Arc::new(
            loader::parse(
                source,
                &root.join("config.toml"),
                &LoadContext {
                    home: root.join("home"),
                    runtime_dir: None,
                    uid: users::get_current_uid(),
                    protected: vec![],
                },
            )
            .unwrap(),
        );
        let store = Store::open(config.state.path.clone()).await.unwrap();
        Self {
            dir,
            config,
            store,
            clock: Arc::new(ReplayClock::new(20.)),
            slack: Arc::new(Slack::default()),
            fetch: Arc::new(Fetch::default()),
        }
    }
    fn execution(&self, brief: String, fetch: bool) -> Execution {
        let root = self.dir.path();
        let action = json!({"reply":{"text":"Running checks.","status":"complete"},"delegations":[{
            "brief":brief,"machine":"local","workspace":"project","backend":"codex","deliverable":"figures_pdf",
            "fetch_repo":if fetch{"o/r"}else{""},"fetch_ref":if fetch{"HEAD"}else{""}
        }]});
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("HOME".into(), root.join("home").into_os_string()),
            (
                "PARENT_LOG".into(),
                root.join("parent.log").into_os_string(),
            ),
            ("PARENT_ACTION".into(), action.to_string().into()),
            (
                "WORKER_LOG".into(),
                root.join("worker.log").into_os_string(),
            ),
            ("SLACK_USER_TOKEN".into(), "xoxp_private_fixture".into()),
            ("FRIDICA_GITHUB_TOKEN".into(), "ghp_owner_fixture".into()),
        ]);
        let mut execution = Execution::system(
            &self.config,
            self.store.clone(),
            self.clock.clone(),
            Host {
                home: root.join("home"),
                environment,
                ssh_control_directory: root.join("private"),
                parent_temporary_root: root.join("parent-temp"),
            },
        )
        .unwrap();
        execution.fetcher = self.fetch.clone();
        execution
    }
    async fn start(&self, mode: Mode) -> ComposedRuntime<Slack> {
        composition::start(
            self.config.clone(),
            self.store.clone(),
            self.slack.clone(),
            self.clock.clone(),
            Arc::new(SequenceIds::default()),
            mode,
        )
        .await
        .unwrap()
    }
    async fn receive(&self, runtime: &ComposedRuntime<Slack>) {
        let envelope = json!({"envelope_id":"envelope-1","type":"events_api","payload":{
            "type":"event_callback","team_id":"TTEAM","event_id":"event-1","event":{
                "type":"message","user":"UALICE","channel":"CROOM","ts":"100.1",
                "text":"<@UOWNER> check https://team.slack.com/archives/CROOM/p99000002 and https://github.com/o/r/issues/1",
                "files":[{"id":"FNOTE","name":"note.txt","mimetype":"text/plain","size":19,"url_private":"https://files.slack.com/note"}]
            }
        }});
        for _ in 0..2 {
            assert_eq!(
                runtime
                    .slack_receiver()
                    .receive(envelope.to_string().as_bytes())
                    .await
                    .unwrap()
                    .unwrap()
                    .envelope_id,
                "envelope-1"
            );
            assert_eq!(self.scalar("SELECT count(*) FROM obligations").await, 1);
        }
    }
    async fn scalar(&self, sql: &str) -> i64 {
        let sql = sql.to_owned();
        self.store
            .call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
    async fn wait_for(&self, sql: &str, expected: i64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.scalar(sql).await != expected {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn composed_processes_fetch_context_and_artifacts_close_only_after_all_confirmed_deliveries()
{
    composed_artifact_flow(false).await;
}

#[tokio::test]
async fn ssh_worker_artifacts_reach_confirmed_delivery_through_shared_composition() {
    composed_artifact_flow(true).await;
}

async fn composed_artifact_flow(remote: bool) {
    let mut f = Fixture::new().await;
    if remote {
        std::fs::set_permissions(
            f.dir.path().join("private"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        let machine = &mut Arc::make_mut(&mut f.config).machines.machines[0];
        machine.transport = "ssh".into();
        machine.host = "owner@fixture".into();
        let path = f.dir.path().join("bin/ssh");
        std::fs::write(&path,"#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@fixture || exit 255\nshift\nexec /bin/sh -c \"$1\"\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let artifact = f.dir.path().join("project/worker1/result.png");
    let runtime = f
        .start(Mode::Active(Box::new(
            f.execution(format!("ARTIFACT:{}", artifact.display()), true),
        )))
        .await;
    assert!(
        !f.dir.path().join("parent.log").exists(),
        "construction must not execute adapters"
    );
    f.receive(&runtime).await;
    let first = runtime.pass().await.unwrap();
    assert_eq!((first.turns, first.started, first.delivered), (1, 1, 1));
    f.wait_for("SELECT count(*) FROM jobs WHERE status='done'", 1)
        .await;
    assert_eq!(
        f.scalar("SELECT count(*) FROM obligations WHERE state='open'")
            .await,
        1
    );
    f.slack.throttle_upload.store(true, Ordering::SeqCst);
    runtime.pass().await.unwrap();
    assert_eq!(
        f.scalar("SELECT count(*) FROM obligations WHERE state='awaiting_delivery'")
            .await,
        1
    );
    assert_eq!(
        f.scalar("SELECT count(*) FROM artifacts WHERE status='ready'")
            .await,
        1
    );
    f.clock.set(22.);
    runtime.pass().await.unwrap();
    assert_eq!(
        f.scalar("SELECT count(*) FROM obligations WHERE state='answered'")
            .await,
        1
    );
    assert_eq!(runtime.pass().await.unwrap().delivered, 0);
    runtime.close().await.unwrap();
    assert!(runtime.processes().await.is_empty());
    assert_eq!(f.slack.downloads.load(Ordering::SeqCst), 1);
    assert_eq!(f.slack.links.load(Ordering::SeqCst), 1);
    assert_eq!(f.fetch.0.lock().unwrap().len(), 1);
    assert!(f.dir.path().join("home/gh-called").exists());
    let parent = std::fs::read_to_string(f.dir.path().join("parent.log")).unwrap();
    assert_eq!(
        parent.lines().count(),
        1,
        "single-job report must bypass parent"
    );
    for text in ["attachment evidence", "linked evidence", "Synthetic issue"] {
        assert!(parent.contains(text), "missing {text}");
    }
    let worker = std::fs::read_to_string(f.dir.path().join("worker.log")).unwrap();
    assert!(worker.contains("Fridica fetched o/r HEAD"));
    assert!(worker.contains("owner-fridica"));
    assert!(!worker.contains("xoxp_private_fixture"));
    let events:Vec<Value>=f.store.call(|c|{
        let rows:Vec<String>=c.prepare("SELECT json_object('seq',seq,'kind',kind,'time',time,'payload',json(payload_json),'complete',complete) FROM replay_events ORDER BY seq")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        Ok(rows.iter().map(|s|serde_json::from_str(s).unwrap()).collect())
    }).await.unwrap();
    for kind in [
        "slack_envelope",
        "intake",
        "parent_call",
        "parent_result",
        "parent_attachment_call",
        "parent_attachment_result",
        "parent_transport_call",
        "parent_transport_result",
        "github_api_call",
        "github_api_result",
        "repo_fetch",
        "worker_call",
        "backend_wire",
        "worker_completion",
        "delivery_call",
        "delivery",
    ] {
        assert!(events.iter().any(|e| e["kind"] == kind), "missing {kind}");
    }
    assert!(events.iter().all(|e| e["complete"] == 1));
    assert!(events
        .windows(2)
        .all(|e| e[0]["seq"].as_i64() < e[1]["seq"].as_i64()));
    let payload = |kind: &str| &events.iter().find(|e| e["kind"] == kind).unwrap()["payload"];
    let output: Vec<u8> =
        serde_json::from_value(payload("parent_transport_result")["output"]["stdout"].clone())
            .unwrap();
    assert_eq!(
        fridica::parent::cli::parse("claude", &output).unwrap(),
        payload("parent_result")["response"],
        "the capture must retain the complete response, not just the chosen action",
    );
    assert_eq!(payload("repo_fetch")["result"]["commit"], "a".repeat(40));
    let completion: fridica::store::work::Completion =
        serde_json::from_value(payload("worker_completion")["completion"].clone()).unwrap();
    assert_eq!(completion.outcome.unwrap().result.report, "Done.");
    assert_eq!(
        completion.artifacts[0].data.as_deref(),
        Some(b"\x89PNG\r\n\x1a\nfixture".as_slice())
    );
    let outcomes: Vec<DeliveryOutcome> = events
        .iter()
        .filter(|e| e["kind"] == "delivery")
        .map(|e| serde_json::from_value(e["payload"]["raw_result"].clone()).unwrap())
        .collect();
    assert_eq!(outcomes.len(), 4);
    assert_eq!(
        outcomes[2],
        DeliveryOutcome::RateLimited { retry_after: 1. }
    );
    assert!(matches!(outcomes[3], DeliveryOutcome::Sent { .. }));
    assert!(!serde_json::to_string(&events)
        .unwrap()
        .contains("ghp_owner_fixture"));
    let posts = f.slack.sent.lock().unwrap();
    assert_eq!(posts.len(), 4); // acknowledgement, report, rate-limited upload, confirmed upload
    assert_eq!(posts[2].id, posts[3].id);
    assert_eq!(
        posts[3].post.blob.as_deref(),
        Some(b"\x89PNG\r\n\x1a\nfixture".as_slice())
    );
}

#[tokio::test]
async fn composed_worker_approval_uses_the_runtime_owner_broker() {
    let f = Fixture::new().await;
    let runtime = f
        .start(Mode::Active(Box::new(
            f.execution("APPROVE:echo".into(), false),
        )))
        .await;
    f.receive(&runtime).await;
    assert_eq!(runtime.pass().await.unwrap().started, 1);
    f.wait_for("SELECT count(*) FROM approvals WHERE status='pending'", 1)
        .await;
    let id: String = f
        .store
        .call(|c| Ok(c.query_row("SELECT id FROM approvals", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(runtime
        .approvals
        .decide(id.clone(), ApprovalDecision::Once, Authority::System)
        .await
        .is_err());
    assert!(runtime
        .approvals
        .decide(id, ApprovalDecision::Once, Authority::Owner)
        .await
        .unwrap());
    f.wait_for("SELECT count(*) FROM jobs WHERE status='done'", 1)
        .await;
    runtime.pass().await.unwrap();
    assert_eq!(
        f.scalar("SELECT count(*) FROM obligations WHERE state='answered'")
            .await,
        1
    );
    assert!(std::fs::read_to_string(f.dir.path().join("worker.log"))
        .unwrap()
        .contains("approval-answer"));
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn shared_observer_and_owner_paused_composition_never_start_external_work() {
    for observe in [true, false] {
        let f = Fixture::new().await;
        let mode = if observe {
            Mode::ObserveOnly
        } else {
            Mode::Active(Box::new(f.execution("Run checks".into(), false)))
        };
        let runtime = f.start(mode).await;
        f.receive(&runtime).await;
        if !observe {
            runtime
                .control(
                    SESSION.into(),
                    Control::Pause {
                        reason: "owner pause".into(),
                    },
                    Authority::Owner,
                )
                .await
                .unwrap();
            assert!(runtime
                .control(SESSION.into(), Control::Resume, Authority::System)
                .await
                .is_err());
        }
        for _ in 0..2 {
            let pass = runtime.pass().await.unwrap();
            assert_eq!((pass.started, pass.delivered), (0, 0));
        }
        assert!(!f.dir.path().join("parent.log").exists());
        assert!(!f.dir.path().join("worker.log").exists());
        assert!(!f.dir.path().join("home/gh-called").exists());
        assert_eq!(f.slack.downloads.load(Ordering::SeqCst), 0);
        assert_eq!(f.slack.links.load(Ordering::SeqCst), 0);
        assert!(f.fetch.0.lock().unwrap().is_empty());
        assert!(f.slack.sent.lock().unwrap().is_empty());
        assert_eq!(
            f.scalar("SELECT count(*) FROM obligations WHERE state='answered'")
                .await,
            0
        );
        runtime.close().await.unwrap();
    }
}

#[tokio::test]
async fn enabled_context_cannot_silently_lose_its_adapter() {
    let f = Fixture::new().await;
    let mut execution = f.execution("Run checks".into(), false);
    execution.github = None;
    assert!(composition::start(
        f.config.clone(),
        f.store.clone(),
        f.slack.clone(),
        f.clock.clone(),
        Arc::new(SequenceIds::default()),
        Mode::Active(Box::new(execution))
    )
    .await
    .is_err());
    assert!(!f.dir.path().join("parent.log").exists());
    assert_eq!(f.scalar("SELECT count(*) FROM replay_events").await, 0);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn composed_isolation_refusal_is_durable_and_precedes_fetch_and_backend_startup() {
    let mut f = Fixture::new().await;
    Arc::make_mut(&mut f.config).machines.machines[0].workspaces[0]
        .policy
        .gpu_confine = Some(true);
    std::fs::create_dir(f.dir.path().join("home/.codex")).unwrap();
    std::fs::write(
        f.dir.path().join("home/.codex/config.toml"),
        "invalid private-value",
    )
    .unwrap();
    let runtime = f
        .start(Mode::Active(Box::new(
            f.execution("Run checks".into(), true),
        )))
        .await;
    f.receive(&runtime).await;
    assert_eq!(runtime.pass().await.unwrap().started, 1);
    f.wait_for("SELECT count(*) FROM jobs WHERE status='failed' AND error='worker_isolation_settings_refused'", 1).await;
    assert_eq!(f.scalar("SELECT count(*) FROM jobs").await, 1);
    assert_eq!(
        f.scalar("SELECT count(*) FROM replay_events WHERE kind='worker_completion'")
            .await,
        1
    );
    assert_eq!(f.scalar("SELECT count(*) FROM replay_events WHERE kind IN ('worker_call','repo_fetch','backend_wire')").await, 0);
    assert!(f.fetch.0.lock().unwrap().is_empty());
    assert!(!f.dir.path().join("worker.log").exists());
    assert!(!f.dir.path().join("project/worker1").exists());
    runtime.pass().await.unwrap();
    assert!(f
        .slack
        .sent
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.post.text.contains("worker_isolation_settings_refused")));
    assert!(!serde_json::to_string(&*f.slack.sent.lock().unwrap())
        .unwrap()
        .contains("private-value"));
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn composition_rejects_stale_mcp_options_before_recovery_mutates_state() {
    let mut f = Fixture::new().await;
    let execution = f.execution("Run checks".into(), false);
    Arc::make_mut(&mut f.config)
        .isolation
        .mcp_aliases
        .push("new-owner-wrapper".into());
    f.store.call(|c| {
        c.execute("INSERT INTO threads(id,workspace,channel,root_ts,status,created,updated) VALUES(?,'TTEAM','CROOM','100.1','active',1,1)", [SESSION])?;
        c.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,state,created) VALUES(?,'message','interrupted','{}','processing',1)", [SESSION])?;
        Ok(())
    }).await.unwrap();
    let result = composition::start(
        f.config.clone(),
        f.store.clone(),
        f.slack.clone(),
        f.clock.clone(),
        Arc::new(SequenceIds::default()),
        Mode::Active(Box::new(execution)),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        f.scalar("SELECT count(*) FROM thread_inbox WHERE state='processing'")
            .await,
        1
    );
    assert!(!f.dir.path().join("parent.log").exists());
    assert!(!f.dir.path().join("worker.log").exists());
}

#[tokio::test]
async fn unrestricted_mcp_settings_refusal_is_durable_without_backend_start_or_retry() {
    let f = Fixture::new().await;
    std::fs::create_dir(f.dir.path().join("home/.codex")).unwrap();
    std::fs::write(
        f.dir.path().join("home/.codex/config.toml"),
        "malformed private-source",
    )
    .unwrap();
    let runtime = f
        .start(Mode::Active(Box::new(
            f.execution("Run checks".into(), false),
        )))
        .await;
    f.receive(&runtime).await;
    assert_eq!(runtime.pass().await.unwrap().started, 1);
    f.wait_for(
        "SELECT count(*) FROM jobs WHERE status='failed' AND error='worker_mcp_settings_refused'",
        1,
    )
    .await;
    assert_eq!(f.scalar("SELECT count(*) FROM jobs").await, 1);
    assert_eq!(f.scalar("SELECT attempt FROM jobs").await, 1);
    assert!(!f.dir.path().join("worker.log").exists());
    runtime.pass().await.unwrap();
    assert!(f
        .slack
        .sent
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.post.text.contains("worker_mcp_settings_refused")));
    assert!(!serde_json::to_string(&*f.slack.sent.lock().unwrap())
        .unwrap()
        .contains("private-source"));
    runtime.close().await.unwrap();
}

#[tokio::test]
async fn live_configuration_rebuilds_real_parent_adapter_and_prompt_limits() {
    let f = Fixture::new().await;
    let mut execution = f.execution("Unused worker".into(), false);
    execution.parent.environment.insert(
        "PARENT_ACTION".into(),
        json!({"reply":{"text":"Updated settings applied.","status":"complete"}})
            .to_string()
            .into(),
    );
    let runtime = f.start(Mode::Active(Box::new(execution))).await;
    runtime
        .update_configuration(
            "parent",
            json!({"model":"fixture-new-model", "triage_model":"fixture-fast"}),
            Authority::Owner,
        )
        .await
        .unwrap();
    runtime
        .update_configuration(
            "limits",
            json!({"max_delegations_per_turn":1}),
            Authority::Owner,
        )
        .await
        .unwrap();
    f.receive(&runtime).await;
    runtime.pass().await.unwrap();
    let log = std::fs::read_to_string(f.dir.path().join("parent.log")).unwrap();
    let calls: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(!calls.is_empty());
    assert!(calls.iter().any(|call| call["argv"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v == "fixture-new-model")));
    assert!(calls.iter().all(|call| call["prompt"]
        .as_str()
        .unwrap()
        .contains("\"max_delegations\":1")));
    // The decide schema is built from the live snapshot: the granted repository
    // is the only non-empty fetch_repo, and no worker IDs exist yet.
    let decide = calls
        .iter()
        .map(|call| {
            let argv = call["argv"].as_array().unwrap();
            let at = argv.iter().position(|v| v == "--json-schema").unwrap();
            serde_json::from_str::<Value>(argv[at + 1].as_str().unwrap()).unwrap()
        })
        .find(|schema| schema["properties"].get("delegations").is_some())
        .unwrap();
    let delegation = &decide["properties"]["delegations"]["items"]["properties"];
    assert_eq!(delegation["fetch_repo"]["enum"], json!(["", "o/r"]));
    assert_eq!(delegation["worker_id"]["enum"], json!([""]));
    assert_eq!(f.scalar("SELECT count(*) FROM jobs").await, 0);
    assert_eq!(
        f.scalar(
            "SELECT count(*) FROM replay_events WHERE kind='configuration_edit' AND complete=0"
        )
        .await,
        0
    );
    assert!(!f.dir.path().join("worker.log").exists());
    runtime.close().await.unwrap();
}
