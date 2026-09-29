use super::*;
use crate::core::time::ReplayClock;
use std::os::unix::fs::PermissionsExt;
const SCRIPT: &str = r#"#!/usr/bin/env python3
import json, os, pathlib, signal, sys, time
root=pathlib.Path(__file__).parent
spec=json.loads((root/'response').read_text())
if spec.get('stubborn'): signal.signal(signal.SIGTERM, signal.SIG_IGN)
with (root/'calls').open('a') as log:
    log.write(json.dumps({'pid':os.getpid(),'argv':sys.argv[1:],'cwd':os.getcwd(),'env':dict(os.environ)})+'\n')
if spec.get('stubborn'): time.sleep(600)
if spec.get('hang'):
    time.sleep(0.4)
    (root/'survived').write_text('bad')
    time.sleep(600)
if spec.get('oversize'):
    sys.stdout.write('x'*(5*1024*1024));sys.stdout.flush();sys.exit(0)
sys.stdout.write(spec['stdout'])
sys.stdout.flush()
sys.stderr.write(spec.get('stderr',''))
sys.exit(spec.get('exit',0))
"#;
struct Harness {
    dir: tempfile::TempDir,
    store: Store,
    clock: Arc<ReplayClock>,
}
impl Harness {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        std::fs::write(dir.path().join("gh"), SCRIPT).unwrap();
        std::fs::set_permissions(
            dir.path().join("gh"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        Self {
            dir,
            store,
            clock: Arc::new(ReplayClock::new(1000.)),
        }
    }
    fn gh(&self, timeout: Duration) -> Gh {
        let (env, secrets) = environment(
            [
                ("HOME".into(), self.dir.path().as_os_str().to_owned()),
                ("PATH".into(), std::env::var_os("PATH").unwrap()),
                ("MY_GH_TOKEN".into(), "ghp_synthetic_secret".into()),
                ("GH_DEBUG".into(), "api".into()),
                ("GH_REPO".into(), "evil/repo".into()),
                ("GH_HOST".into(), "evil.invalid".into()),
                ("FRIDICA_SLACK_TOKEN".into(), "xoxp_synthetic_slack".into()),
                ("SSH_AUTH_SOCK".into(), "/fake/agent".into()),
            ],
            "MY_GH_TOKEN",
        );
        Gh {
            store: self.store.clone(),
            clock: self.clock.clone(),
            options: Options {
                program: self.dir.path().join("gh"),
                timeout,
            },
            env,
            secrets,
            permits: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    fn response(&self, value: Value) {
        std::fs::write(self.dir.path().join("response"), value.to_string()).unwrap();
    }
    fn calls(&self) -> Vec<Value> {
        std::fs::read_to_string(self.dir.path().join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect()
    }
    async fn scalar(&self, query: &str) -> i64 {
        let query = query.to_owned();
        self.store
            .call(move |c| Ok(c.query_row(&query, [], |r| r.get(0))?))
            .await
            .unwrap()
    }
    async fn ledger(&self) -> String {
        self.store
            .call(|c| {
                Ok(c.query_row(
                    "SELECT group_concat(payload_json) FROM replay_events",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap()
    }
}
fn request() -> Request {
    Request {
        repo: "o/r".into(),
        operation: Operation::Issue { number: 1 },
    }
}
fn response(status: u16, headers: &str, body: &str) -> Value {
    json!({"stdout":format!("HTTP/2.0 {status} TEST\r\n{headers}\r\n{body}"),"exit":if status>=400{1}else{0}})
}
#[tokio::test]
async fn gh_reads_use_fixed_gets_private_cwd_owner_credentials_and_redacted_records() {
    let h = Harness::new().await;
    let gh = h.gh(Duration::from_secs(2));
    let mut reply = response(
        200,
        "",
        r#"{"number":1,"body":"ghp_synthetic_secret xoxp-another-secret github_pat_echo"}"#,
    );
    reply["stderr"] = json!("token ghp_synthetic_secret");
    h.response(reply);
    let result = gh.get(request()).await.unwrap();
    assert_eq!(result["body"], "[redacted] [redacted] [redacted]");
    let calls = h.calls();
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(
        call["argv"],
        json!([
            "api",
            "--hostname",
            "github.com",
            "--method",
            "GET",
            "--include",
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
            "/repos/o/r/issues/1"
        ])
    );
    assert_eq!(call["env"]["GH_TOKEN"], "ghp_synthetic_secret");
    assert_eq!(call["env"]["GH_PROMPT_DISABLED"], "1");
    for key in [
        "GH_DEBUG",
        "GH_REPO",
        "GH_HOST",
        "FRIDICA_SLACK_TOKEN",
        "SSH_AUTH_SOCK",
        "MY_GH_TOKEN",
    ] {
        assert!(call["env"].get(key).is_none());
    }
    assert_ne!(
        call["cwd"].as_str().unwrap(),
        std::env::current_dir().unwrap().to_str().unwrap()
    );
    assert!(!std::path::Path::new(call["cwd"].as_str().unwrap()).exists());
    let ledger = h.ledger().await;
    assert!(!ledger.contains("synthetic_secret"));
    assert!(!ledger.contains("another-secret"));
    assert!(!ledger.contains("github_pat_echo"));
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='github_api_call' AND complete=1")
            .await,
        1
    );
}
#[tokio::test]
async fn gh_rate_limit_survives_adapter_restart_and_html_error_bodies() {
    let h = Harness::new().await;
    h.response(response(
        403,
        "Retry-After: 42\r\nX-RateLimit-Remaining: 12\r\n",
        "<html>limited</html>",
    ));
    assert_eq!(
        h.gh(Duration::from_secs(2)).get(request()).await,
        Err(Failure::RateLimited { after: 42. })
    );
    h.clock.set(1010.);
    assert_eq!(
        h.gh(Duration::from_secs(2)).get(request()).await,
        Err(Failure::RateLimited { after: 32. })
    );
    assert_eq!(h.calls().len(), 1);
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='github_api_deferred'")
            .await,
        1
    );
    h.clock.set(1043.);
    h.response(response(200, "", r#"{"number":1}"#));
    assert!(h.gh(Duration::from_secs(2)).get(request()).await.is_ok());
    assert_eq!(h.calls().len(), 2);
}
#[tokio::test]
async fn gh_errors_malformed_output_and_process_limits_are_truthful() {
    for (reply, expected) in [
        (response(404, "", "{}"), Failure::NotFound),
        (
            response(403, "X-RateLimit-Remaining: 12\r\n", "{}"),
            Failure::Http { status: 403 },
        ),
        (response(200, "", "not JSON"), Failure::Invalid),
        (json!({"stdout":"","exit":1}), Failure::Invalid),
        (json!({"stdout":"","exit":4}), Failure::Authentication),
        (json!({"oversize":true}), Failure::Unavailable),
        (json!({"hang":true}), Failure::Unavailable),
    ] {
        let h = Harness::new().await;
        // Leave time for interpreter startup (slow on macOS) but stay below the
        // script's 0.4 s delay before writing `survived`.
        let timeout = if reply["hang"] == true {
            Duration::from_millis(300)
        } else {
            Duration::from_secs(2)
        };
        h.response(reply);
        assert_eq!(h.gh(timeout).get(request()).await, Err(expected));
        assert_eq!(h.calls().len(), 1);
        assert!(!h.dir.path().join("survived").exists());
    }
}
#[tokio::test]
async fn gh_recording_faults_prevent_spawn_or_keep_intent_incomplete() {
    for after in [false, true] {
        let h = Harness::new().await;
        h.response(response(200, "", "{}"));
        let kind = if after {
            "github_api_result"
        } else {
            "github_api_call"
        };
        h.store.call(move|c|{c.execute_batch(&format!("CREATE TRIGGER fail_github BEFORE INSERT ON replay_events WHEN NEW.kind='{kind}' BEGIN SELECT RAISE(ABORT,'private SQL'); END;"))?;Ok(())}).await.unwrap();
        assert_eq!(
            h.gh(Duration::from_secs(2)).get(request()).await,
            Err(Failure::Recording)
        );
        assert_eq!(h.calls().len(), usize::from(after));
        assert_eq!(
            h.scalar(
                "SELECT count(*) FROM replay_events WHERE kind='github_api_call' AND complete=0"
            )
            .await,
            i64::from(after)
        );
    }
}
#[tokio::test]
async fn gh_cancellation_preserves_unfinished_intent_and_terminates_child() {
    let h = Harness::new().await;
    h.response(json!({"hang":true}));
    let gh = h.gh(Duration::from_secs(8));
    let task = tokio::spawn(async move { gh.get(request()).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert!(!h.dir.path().join("survived").exists());
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='github_api_call' AND complete=0")
            .await,
        1
    );
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='github_api_result'")
            .await,
        0
    );
}
#[test]
fn gh_rate_delay_and_environment_selection_do_not_request_new_credentials() {
    for (headers, expected) in [
        (BTreeMap::from([("retry-after".into(), "42".into())]), 42.),
        (
            BTreeMap::from([("x-ratelimit-reset".into(), "1060".into())]),
            60.,
        ),
        (BTreeMap::from([("retry-after".into(), "NaN".into())]), 300.),
        (
            BTreeMap::from([("retry-after".into(), "99999".into())]),
            3600.,
        ),
    ] {
        assert_eq!(retry_after(&headers, 1000.), expected);
    }
    let (env, _) = environment(
        [
            ("GH_TOKEN".into(), "existing".into()),
            ("MY_GH_TOKEN".into(), "legacy".into()),
        ],
        "MY_GH_TOKEN",
    );
    assert_eq!(env[std::ffi::OsStr::new("GH_TOKEN")], "existing");
}

#[tokio::test]
async fn gh_process_concurrency_is_bounded_and_waiters_cancel_without_spawning() {
    let h = Harness::new().await;
    h.response(json!({"hang":true}));
    let gh = Arc::new(h.gh(Duration::from_secs(8)));
    let mut tasks = vec![];
    for _ in 0..6 {
        let gh = gh.clone();
        tasks.push(tokio::spawn(async move { gh.get(request()).await }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().len() < 4 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.calls().len(), 4);
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        assert!(task.await.unwrap_err().is_cancelled());
    }
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(h.calls().len(), 4);
    assert!(!h.dir.path().join("survived").exists());
    assert_eq!(
        h.scalar("SELECT count(*) FROM replay_events WHERE kind='github_api_call' AND complete=0")
            .await,
        4
    );
}

#[tokio::test]
async fn gh_permit_is_not_reused_until_cancelled_process_cleanup_finishes() {
    let h = Harness::new().await;
    h.response(json!({"stubborn":true}));
    let gh = Arc::new(h.gh(Duration::from_secs(8)));
    let mut tasks = vec![];
    // Establish which caller owns the first process before filling the other slots.
    let first = gh.clone();
    tasks.push(tokio::spawn(async move { first.get(request()).await }));
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    for _ in 0..3 {
        let gh = gh.clone();
        tasks.push(tokio::spawn(async move { gh.get(request()).await }));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().len() < 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let pid = rustix::process::Pid::from_raw(h.calls()[0]["pid"].as_i64().unwrap() as i32).unwrap();
    // Observe the resource being tested directly. Starting a fifth gh request
    // also waits for SQLite intent fsync and interpreter startup; neither is a
    // cleanup acknowledgement and either may lag after the permit is released.
    let permits = gh.permits.clone();
    let waiting = tokio::spawn(async move { permits.acquire_owned().await.unwrap() });
    tasks[0].abort();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.calls().len(), 4);
    assert!(
        !waiting.is_finished(),
        "permit returned before stubborn process cleanup"
    );
    let permit = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH),
        "leader must be reaped before permit reuse"
    );
    drop(permit);
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        assert!(task.await.unwrap_err().is_cancelled());
    }
    tokio::time::timeout(Duration::from_secs(3), async {
        while gh.permits.available_permits() != 4 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[test]
fn gh_redaction_handles_dense_token_prefixes_and_proxy_credentials() {
    let raw = json!({"body":"ghp_fake ".repeat(20_000)}).to_string();
    let cleaned: Value = serde_json::from_str(&redact(&raw, &[])).unwrap();
    assert_eq!(cleaned["body"], "[redacted] ".repeat(20_000));
    let (_, secrets) = environment(
        [(
            "HTTPS_PROXY".into(),
            "https://owner:proxy-private@proxy.invalid".into(),
        )],
        "MY_GH_TOKEN",
    );
    assert_eq!(
        redact(
            "https://owner:proxy-private@proxy.invalid proxy-private",
            &secrets
        ),
        "[redacted] [redacted]"
    );
}

#[tokio::test]
async fn next_request_recording_can_stall_after_cancelled_process_cleanup_has_finished() {
    let h = Harness::new().await;
    h.response(json!({"stubborn":true}));
    let mut client = h.gh(Duration::from_secs(8));
    client.permits = Arc::new(tokio::sync::Semaphore::new(1));
    let gh = Arc::new(client);
    let first = gh.clone();
    let first = tokio::spawn(async move { first.get(request()).await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let pid = rustix::process::Pid::from_raw(h.calls()[0]["pid"].as_i64().unwrap() as i32).unwrap();
    let (release, blocked) = std::sync::mpsc::channel();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let store = h.store.clone();
    let blocker = tokio::spawn(async move {
        store
            .call(move |_| {
                entered.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(10))?;
                Ok(())
            })
            .await
            .unwrap()
    });
    ready.await.unwrap();
    let permits = gh.permits.clone();
    let waiting = tokio::spawn(async move { permits.acquire_owned().await.unwrap() });
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    let permit = tokio::time::timeout(Duration::from_secs(3), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    drop(permit);
    let next = gh.clone();
    let next = tokio::spawn(async move { next.get(request()).await });
    // Reproduce the old witness timing out even though cleanup is proven done.
    assert!(tokio::time::timeout(Duration::from_secs(3), async {
        while h.calls().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .is_err());
    assert!(!next.is_finished());
    release.send(()).unwrap();
    blocker.await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.calls().len() < 2 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    next.abort();
    assert!(next.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(3), async {
        while gh.permits.available_permits() != 1 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
