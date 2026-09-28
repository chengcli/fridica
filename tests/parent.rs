use fridica::{
    attention::{self, Message},
    config::{loader, Config, LoadContext},
    core::{
        parent::{Parent, ParentRequest},
        time::{ReplayClock, SequenceIds},
        Authority,
    },
    parent::{self, CliParent, Options},
    store::Store,
    threads::{
        actor::{Actor, Step},
        controls::{self, Control},
    },
};
use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
const SESSION: &str = "TTEAM:CROOM:100.1";
fn normalize(argv: Vec<String>) -> Vec<Value> {
    argv.iter()
        .enumerate()
        .map(|(i, v)| {
            if i > 0 && matches!(argv[i - 1].as_str(), "--settings" | "--json-schema") {
                serde_json::from_str(v).unwrap()
            } else {
                json!(v)
            }
        })
        .collect()
}
#[test]
fn frozen_commands_envelopes_and_context_budgets_match_with_declared_rejections() {
    let corpus: Value = serde_json::from_str(include_str!("corpus/parent.json")).unwrap();
    assert_eq!(parent::schema::triage(), corpus["schema"]);
    for case in corpus["commands"].as_array().unwrap() {
        let backend = case["backend"].as_str().unwrap();
        let actual = parent::cli::command(
            backend,
            backend,
            Path::new("/private-parent"),
            &corpus["schema"],
            case["model"].as_str().unwrap(),
            case["effort"].as_str().unwrap(),
        )
        .unwrap();
        assert_eq!(
            normalize(actual),
            normalize(serde_json::from_value(case["expected"].clone()).unwrap())
        );
    }
    for case in corpus["envelopes"].as_array().unwrap() {
        let result = parent::cli::parse(
            case["backend"].as_str().unwrap(),
            case["output"].as_str().unwrap().as_bytes(),
        );
        if !case["rust_rejection"].is_null() {
            assert!(!case["expected"].is_null(), "stale exception");
            assert!(result.is_err(), "{}", case["rust_rejection"]);
        } else if case["expected"].is_null() {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), case["expected"]);
        }
    }
    for case in corpus["budgets"].as_array().unwrap() {
        let mut views = case["views"].as_array().unwrap().clone();
        let budget = case["budget"].as_u64().unwrap() as usize;
        assert_eq!(
            parent::context::fit_attachments(&mut views, budget),
            case["remaining"].as_u64().unwrap() as usize
        );
        assert_eq!(json!(views), case["expected"], "budget={budget}");
        assert_eq!(
            json!(parent::context::bounded(
                case["history"].as_array().unwrap(),
                budget
            )),
            case["bounded"]
        );
    }
}
fn request(call: &str) -> ParentRequest {
    ParentRequest {
        inbox_id: 1,
        call: call.into(),
        session: json!({"id":SESSION,"channel":"CROOM","work":{"workers":[],"busy":{}},"machines":[],"status":"new"}),
        trigger: json!({"kind":"message","message":{"text":"Hello"}}),
        history: vec![],
        obligations: vec![],
        previous: None,
        errors: vec![],
    }
}
struct Harness {
    dir: tempfile::TempDir,
    store: Store,
    config: Arc<Config>,
    clock: Arc<ReplayClock>,
    parent: Arc<CliParent>,
}
impl Harness {
    async fn new(backend: &str, source: &str, mode: &str, timeout: f64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("temp")).unwrap();
        let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
        let mut config = loader::parse(
            corpus["source"].as_str().unwrap(),
            &dir.path().join("config.toml"),
            &LoadContext {
                home: dir.path().into(),
                uid: 1,
                runtime_dir: None,
                protected: vec![],
            },
        )
        .unwrap();
        config.parent.backend = backend.into();
        config.parent.timeout = timeout;
        config.parent.model = "main-model".into();
        config.parent.triage_model = "triage-model".into();
        let executable = dir.path().join(backend);
        std::fs::write(&executable, source).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = Arc::new(ReplayClock::new(20.));
        let config = Arc::new(config);
        let options = Options {
            codex: executable.to_string_lossy().into(),
            claude: executable.to_string_lossy().into(),
            temporary_root: dir.path().join("temp"),
            environment: [
                ("PATH", std::env::var("PATH").unwrap()),
                ("MODE", mode.into()),
                ("LLM_LOG", dir.path().join("log").to_string_lossy().into()),
                ("SLACK_USER_TOKEN", "xoxp-private-test".into()),
                ("FRIDICA_GITHUB_TOKEN", "private-gh-test".into()),
            ]
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect(),
        };
        let parent = Arc::new(CliParent::new(
            config.clone(),
            store.clone(),
            clock.clone(),
            options,
        ));
        Self {
            dir,
            store,
            config,
            clock,
            parent,
        }
    }
    fn actor(&self) -> Actor<CliParent> {
        Actor {
            config: Some(self.config.clone()),
            store: self.store.clone(),
            parent: self.parent.clone(),
            clock: self.clock.clone(),
            ids: Arc::new(SequenceIds::default()),
            owner: "UOWNER".into(),
            limits: self.config.attention.clone(),
            observe_only: false,
            parent_timeout: Duration::from_secs(5),
        }
    }
    async fn intake(&self, n: usize, text: &str) -> String {
        attention::intake(
            &self.store,
            Message {
                event_id: format!("e{n}"),
                workspace: "TTEAM".into(),
                channel: "CROOM".into(),
                ts: format!("100.{n}"),
                thread_ts: None,
                sender: "UALICE".into(),
                text: text.into(),
                source: "socket".into(),
                meta: None,
                attachments: vec![],
            },
            "UOWNER".into(),
            20.,
            900.,
            format!("o{n}"),
        )
        .await
        .unwrap();
        format!("TTEAM:CROOM:100.{n}")
    }
    async fn scalar(&self, sql: &str) -> String {
        let sql = sql.to_owned();
        self.store
            .call(move |c| Ok(c.query_row(&sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
    fn clean(&self) {
        assert_eq!(
            std::fs::read_dir(self.dir.path().join("temp"))
                .unwrap()
                .count(),
            0
        );
    }
}
#[tokio::test]
async fn frozen_fake_clis_run_statelessly_with_scrubbed_environment_and_durable_records() {
    for (backend, source, choice) in [
        (
            "codex",
            include_str!("corpus/fake_parent_codex.py"),
            "ignore",
        ),
        (
            "claude",
            include_str!("corpus/fake_parent_claude.py"),
            "respond",
        ),
    ] {
        let h = Harness::new(backend, source, "ok", 5.).await;
        assert_eq!(
            h.parent.decide(request("triage")).await.unwrap(),
            json!({"decision":choice})
        );
        h.clean();
        let log: Value =
            serde_json::from_slice(&std::fs::read(h.dir.path().join("log")).unwrap()).unwrap();
        assert!(log["slack"].is_null());
        let argv = log["argv"].as_array().unwrap();
        let model = argv.iter().position(|x| x == "--model").unwrap();
        assert_eq!(argv[model + 1], "triage-model");
        assert!(log["prompt"].as_str().unwrap().contains("untrusted data"));
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE complete=0")
                .await,
            "0"
        );
        assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_transport_result'").await,"1");
    }
}
const FAKE: &str = r#"#!/usr/bin/env python3
import json,os,pathlib,sys,time
args=sys.argv[1:]
prompt=sys.stdin.read()
mode=os.environ.get('MODE','ok')
log=pathlib.Path(os.environ['LLM_LOG'])
backend='codex' if args[0]=='exec' else 'claude'
schema=json.load(open(args[args.index('--output-schema')+1])) if backend=='codex' else json.loads(args[args.index('--json-schema')+1])
with log.open('a') as out: out.write(json.dumps({'args':args,'prompt':prompt,'cwd':os.getcwd(),'mode':oct(pathlib.Path('.').stat().st_mode&0o777),'schema_mode':oct(pathlib.Path('schema.json').stat().st_mode&0o777),'pid':os.getpid(),'slack':os.environ.get('SLACK_USER_TOKEN'),'github':os.environ.get('FRIDICA_GITHUB_TOKEN')})+'\n')
if mode=='hang': time.sleep(30)
if mode.startswith('gate'):
 while not log.with_suffix('.release').exists(): time.sleep(0.01)
if mode=='exit':
 print('PRIVATE xoxp-sensitive',file=sys.stderr);sys.exit(7)
if mode=='flood': print('x'*5000000);sys.exit(0)
if mode=='tools' and backend=='codex':print(json.dumps({'type':'item.started','item':{'type':'command_execution'}}))
if 'decision' in schema['properties']: result={'decision':'ignore' if mode in ('ignore','gate_ignore') else 'respond'}
else:
 result={'reply':{'text':'Done.','status':'complete','answers':[]}}
 if mode=='repair' and not json.loads(prompt.split('\n\nData:\n')[1])['repair']['errors']:result={'unknown':'force repair'}
if backend=='codex':
 print(json.dumps({'type':'item.completed','item':{'type':'agent_message','text':json.dumps(result)}}))
else: print(json.dumps({'is_error':False,'structured_output':result,'permission_denials':[{'tool_name':'Bash'}] if mode=='tools' else []}))
"#;
#[tokio::test]
async fn real_process_parent_triages_then_decides_and_persists_channel_cooldown() {
    for backend in ["codex", "claude"] {
        let h = Harness::new(backend, FAKE, "ok", 5.).await;
        let a = h.actor();
        assert_eq!(
            a.step(h.intake(1, "Can someone check the build?").await)
                .await
                .unwrap(),
            Step::Committed
        );
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM parent_turns")
                .await,
            "2"
        );
        assert_eq!(
            h.scalar("SELECT CAST(last_unsolicited AS TEXT) FROM cooldowns")
                .await,
            "20.0"
        );
        assert_eq!(
            a.step(h.intake(2, "Another question").await).await.unwrap(),
            Step::Observed
        );
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM parent_turns")
                .await,
            "2"
        );
        // Explicit mentions bypass the channel cooldown.
        assert_eq!(
            a.step(h.intake(3, "<@UOWNER> important question").await)
                .await
                .unwrap(),
            Step::Committed
        );
        h.clock.set(81.);
        assert_eq!(
            a.step(h.intake(4, "A later question").await).await.unwrap(),
            Step::Committed
        );
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            "3"
        );
        for line in std::fs::read_to_string(h.dir.path().join("log"))
            .unwrap()
            .lines()
        {
            let log: Value = serde_json::from_str(line).unwrap();
            assert_eq!(log["mode"], "0o700");
            assert_eq!(log["schema_mode"], "0o600");
            assert!(log["slack"].is_null() && log["github"].is_null());
            assert!(!Path::new(log["cwd"].as_str().unwrap()).exists());
        }
        h.clean();
    }
}
#[tokio::test]
async fn triage_ignore_and_unavailable_settle_without_posts_or_failure_loops() {
    for mode in ["ignore", "exit", "tools"] {
        let h = Harness::new("claude", FAKE, mode, 5.).await;
        assert_eq!(
            h.actor()
                .step(h.intake(1, "general message").await)
                .await
                .unwrap(),
            Step::Observed
        );
        assert_eq!(h.scalar("SELECT state FROM thread_inbox").await, "done");
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            "0"
        );
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM obligations")
                .await,
            "0"
        );
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM cooldowns")
                .await,
            "0"
        );
        h.clean();
    }
}
#[tokio::test]
async fn action_repair_uses_full_previous_response_and_does_not_reuse_cli_sessions() {
    let h = Harness::new("codex", FAKE, "repair", 5.).await;
    assert_eq!(
        h.actor()
            .step(h.intake(1, "<@UOWNER> help").await)
            .await
            .unwrap(),
        Step::Committed
    );
    let logs: Vec<Value> = std::fs::read_to_string(h.dir.path().join("log"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(logs.len(), 2);
    assert_ne!(logs[0]["cwd"], logs[1]["cwd"]);
    let prompt = logs[1]["prompt"].as_str().unwrap();
    let data: Value = serde_json::from_str(prompt.split_once("\n\nData:\n").unwrap().1).unwrap();
    assert_eq!(
        data["repair"]["previous_answer"],
        json!({"unknown":"force repair"})
    );
    assert!(!data["repair"]["errors"].as_array().unwrap().is_empty());
    h.clean();
}
#[tokio::test]
async fn parent_exit_tool_attempt_and_oversized_output_fail_with_fixed_diagnostics() {
    for backend in ["codex", "claude"] {
        for (mode, code) in [
            ("exit", "parent_exit_7"),
            ("tools", "parent_tools_attempted"),
            ("flood", "parent_output_limit"),
        ] {
            let h = Harness::new(backend, FAKE, mode, 5.).await;
            assert_eq!(
                h.parent.decide(request("triage")).await.unwrap_err().code,
                code
            );
            h.clean();
            if mode == "flood" {
                assert_eq!(
                    h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE complete=0")
                        .await,
                    "2"
                );
            }
        }
    }
}
#[tokio::test]
async fn observe_only_and_owner_pause_never_start_parent_processes() {
    let h = Harness::new("codex", FAKE, "ok", 5.).await;
    let mut a = h.actor();
    a.observe_only = true;
    assert_eq!(
        a.step(h.intake(1, "<@UOWNER> help").await).await.unwrap(),
        Step::Observed
    );
    a.observe_only = false;
    h.intake(2, "question").await;
    controls::apply(
        &h.store,
        "TTEAM:CROOM:100.2".into(),
        Control::Pause {
            reason: "owner review".into(),
        },
        Authority::Owner,
        20.,
    )
    .await
    .unwrap();
    assert_eq!(
        a.step("TTEAM:CROOM:100.2".into()).await.unwrap(),
        Step::Observed
    );
    assert!(!h.dir.path().join("log").exists());
    h.clean();
}
#[test]
fn owner_prompt_inputs_reload_and_schema_matches_the_implemented_actions() {
    let dir = tempfile::tempdir().unwrap();
    let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
    let mut config = loader::parse(
        corpus["source"].as_str().unwrap(),
        &dir.path().join("config.toml"),
        &LoadContext {
            home: dir.path().into(),
            uid: 1,
            runtime_dir: None,
            protected: vec![],
        },
    )
    .unwrap();
    let rules = dir.path().join("rules.md");
    config.owner.contract = Some(rules.clone());
    std::fs::write(&rules, "## Participation\nTriage A\n## Replies\nRule A").unwrap();
    assert!(parent::prompts::build(&config, &request("decide"))
        .unwrap()
        .0
        .contains("Rule A"));
    std::fs::write(&rules, "## Participation\nTriage B\n## Replies\nRule B").unwrap();
    assert!(parent::prompts::build(&config, &request("triage"))
        .unwrap()
        .0
        .contains("Triage B"));
    std::fs::write(&rules, "invalid").unwrap();
    assert!(parent::prompts::build(&config, &request("decide")).is_err());
    fn strict(v: &Value) {
        if v["type"] == "object" {
            assert_eq!(v["additionalProperties"], false);
            let properties = v["properties"].as_object().unwrap();
            assert_eq!(v["required"].as_array().unwrap().len(), properties.len());
            for child in properties.values() {
                strict(child);
            }
        }
        if let Some(array) = v["anyOf"].as_array() {
            for child in array {
                strict(child);
            }
        }
        if v["type"] == "array" {
            strict(&v["items"]);
        }
    }
    strict(&parent::schema::decision());
}

async fn logs(h: &Harness, n: usize) -> Vec<Value> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let records: Vec<Value> = std::fs::read_to_string(h.dir.path().join("log"))
                .unwrap_or_default()
                .lines()
                .filter_map(|s| serde_json::from_str(s).ok())
                .collect();
            if records.len() >= n {
                return records;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap()
}
async fn gone(pid: u64) {
    tokio::time::timeout(Duration::from_secs(3), async {
        let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
        while rustix::process::test_kill_process(pid).is_ok() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn deadlines_and_cancellation_reap_parent_and_preserve_incomplete_evidence() {
    let h = Harness::new("codex", FAKE, "hang", 0.25).await;
    assert_eq!(
        h.parent.decide(request("triage")).await.unwrap_err().code,
        "parent_timeout"
    );
    let records = logs(&h, 1).await;
    gone(records[0]["pid"].as_u64().unwrap()).await;
    h.clean();
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE complete=0")
            .await,
        "2"
    );
    let h = Harness::new("claude", FAKE, "hang", 30.).await;
    let p = h.parent.clone();
    let call = tokio::spawn(async move { p.decide(request("triage")).await });
    let records = logs(&h, 1).await;
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    gone(records[0]["pid"].as_u64().unwrap()).await;
    h.clean();
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM replay_events WHERE kind='parent_transport_call' AND complete=0").await,"1");
}
#[tokio::test]
async fn owner_controls_during_triage_fence_both_respond_and_quiet_results() {
    for mode in ["gate", "gate_ignore"] {
        let h = Harness::new("codex", FAKE, mode, 5.).await;
        let a = h.actor();
        let session = h.intake(1, "A general question").await;
        let call = tokio::spawn(async move { a.step(session).await });
        logs(&h, 1).await;
        controls::apply(
            &h.store,
            SESSION.into(),
            Control::Pause {
                reason: "stop now".into(),
            },
            Authority::Owner,
            21.,
        )
        .await
        .unwrap();
        std::fs::write(h.dir.path().join("log.release"), "").unwrap();
        assert_eq!(call.await.unwrap().unwrap(), Step::Stale);
        assert_eq!(h.scalar("SELECT state FROM thread_inbox").await, "pending");
        assert_eq!(
            h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
            "0"
        );
        assert_eq!(
            std::fs::read_to_string(h.dir.path().join("log"))
                .unwrap()
                .lines()
                .count(),
            1
        );
        assert_eq!(
            h.actor().step(SESSION.into()).await.unwrap(),
            Step::Observed
        );
        h.clean();
    }
}
#[tokio::test]
async fn concurrent_unsolicited_replies_recheck_channel_cooldown_in_commit() {
    let h = Harness::new("claude", FAKE, "gate", 5.).await;
    let a = h.actor();
    let first = h.intake(1, "One general question").await;
    let second = h.intake(2, "Another question").await;
    let mut b = h.actor();
    b.ids = a.ids.clone();
    let one = tokio::spawn(async move { a.step(first).await });
    let two = tokio::spawn(async move { b.step(second).await });
    logs(&h, 2).await;
    std::fs::write(h.dir.path().join("log.release"), "").unwrap();
    let one = one.await.unwrap().unwrap();
    let two = two.await.unwrap().unwrap();
    assert!(
        (one == Step::Committed && two == Step::Deferred)
            || (two == Step::Committed && one == Step::Deferred)
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM outbox").await,
        "1"
    );
    assert_eq!(h.scalar("SELECT CAST(count(*) AS TEXT) FROM thread_inbox WHERE state='pending' AND not_before=80").await,"1");
    h.clean();
}
#[tokio::test]
async fn recording_failure_prevents_process_start_and_context_failure_stays_private() {
    let h = Harness::new("codex", FAKE, "ok", 5.).await;
    h.store.call(|c|{c.execute_batch("CREATE TEMP TRIGGER block_parent BEFORE INSERT ON replay_events BEGIN SELECT RAISE(ABORT,'disk failure'); END;")?;Ok(())}).await.unwrap();
    assert_eq!(
        h.parent.decide(request("triage")).await.unwrap_err().code,
        "parent_recording_failed"
    );
    assert!(!h.dir.path().join("log").exists());
    h.clean();
}

#[tokio::test]
async fn missing_owner_context_fails_before_spawning_and_disabling_general_messages_preserves_followup_triage(
) {
    let h = Harness::new("codex", FAKE, "ok", 5.).await;
    let mut config = (*h.config).clone();
    config.owner.contract = Some(h.dir.path().join("private-missing-contract"));
    let p = CliParent::new(
        Arc::new(config),
        h.store.clone(),
        h.clock.clone(),
        Options::default(),
    );
    assert_eq!(
        p.decide(request("decide")).await.unwrap_err().code,
        "parent_context_failed"
    );
    assert!(!h.dir.path().join("log").exists());
    let mut a = h.actor();
    let mut config = (*h.config).clone();
    config.slack.general_messages = false;
    a.config = Some(Arc::new(config));
    assert_eq!(
        a.step(h.intake(1, "Unaddressed").await).await.unwrap(),
        Step::Observed
    );
    assert!(!h.dir.path().join("log").exists());
    h.store
        .call(|c| {
            c.execute("UPDATE threads SET turns=1", [])?;
            Ok(())
        })
        .await
        .unwrap();
    attention::intake(
        &h.store,
        Message {
            event_id: "followup".into(),
            workspace: "TTEAM".into(),
            channel: "CROOM".into(),
            ts: "101.1".into(),
            thread_ts: Some("100.1".into()),
            sender: "UALICE".into(),
            text: "One more question".into(),
            source: "socket".into(),
            meta: None,
            attachments: vec![],
        },
        "UOWNER".into(),
        20.,
        900.,
        "unused".into(),
    )
    .await
    .unwrap();
    assert_eq!(a.step(SESSION.into()).await.unwrap(), Step::Committed);
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM parent_turns")
            .await,
        "2"
    );
    assert_eq!(
        h.scalar("SELECT CAST(count(*) AS TEXT) FROM cooldowns")
            .await,
        "0"
    );
    h.clean();
}

#[tokio::test]
async fn quiet_triage_after_recovery_releases_an_unposted_reply_reservation() {
    let h = Harness::new("claude", FAKE, "ignore", 5.).await;
    h.intake(1, "A general question").await;
    attention::reserve(
        &h.store,
        SESSION.into(),
        1,
        "human".into(),
        20.,
        "held".into(),
        h.config.attention.clone(),
    )
    .await
    .unwrap();
    h.store
        .call(|c| {
            c.execute("UPDATE thread_inbox SET state='processing'", [])?;
            Ok(())
        })
        .await
        .unwrap();
    fridica::threads::actor::recover(&h.store).await.unwrap();
    assert_eq!(
        h.actor().step(SESSION.into()).await.unwrap(),
        Step::Observed
    );
    assert_eq!(
        h.scalar("SELECT state FROM reply_reservations").await,
        "released"
    );
    h.clean();
}
