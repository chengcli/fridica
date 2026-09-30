use fridica::{
    config::{loader, Config, LoadContext},
    control::{
        api::target,
        client::{Client, Failure as ClientFailure},
        server::{Access, Capability, Failure, Options, Server},
        Backend, Request, Response, BODY_LIMIT,
    },
    core::{delivery::AdapterFuture, Authority},
};
use serde_json::json;
use std::{
    os::unix::fs::{symlink, PermissionsExt},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::Semaphore,
};
struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let mut config = loader::parse(
            &format!(
                r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
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
                uid: users::get_current_uid(),
                protected: vec![],
            },
        )
        .unwrap();
        config.state.control_socket = dir.path().join("private/control.sock");
        Self { _dir: dir, config }
    }
    async fn bind(&self, backend: Arc<dyn Backend>, access: Access, options: Options) -> Server {
        Server::bind(&self.config, backend, access, options)
            .await
            .unwrap()
    }
    fn client(&self, token: Option<String>) -> Client {
        Client::new(&self.config.state.control_socket, token).unwrap()
    }
    async fn raw(&self, header: &str, body: &[u8]) -> String {
        let mut stream = UnixStream::connect(&self.config.state.control_socket)
            .await
            .unwrap();
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
        let mut data = vec![];
        tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut data))
            .await
            .unwrap()
            .unwrap();
        String::from_utf8(data).unwrap()
    }
}
#[derive(Default)]
struct Echo {
    calls: AtomicUsize,
}
impl Backend for Echo {
    fn request(&self, r: Request, a: Authority) -> AdapterFuture<'_, Response> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Response::ok(json!({"method":r.method,"target":r.target,"body":r.body,"authority":a}))
        })
    }
}
fn rejected(error: ClientFailure, status: u16) {
    assert!(
        matches!(error,ClientFailure::Rejected{status:s,..} if s==status),
        "{error:?}"
    );
}
#[tokio::test]
async fn owner_peer_capabilities_and_cli_derive_authority_without_exposing_secrets() {
    let f = Fixture::new();
    let echo = Arc::new(Echo::default());
    let server = f
        .bind(echo.clone(), Access::OwnerPeer, Options::default())
        .await;
    assert_eq!(
        std::fs::metadata(&f.config.state.control_socket)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        f.client(None)
            .request("GET", "/status", None)
            .await
            .unwrap()["authority"]["kind"],
        "owner"
    );
    let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["status", "--socket"])
        .arg(&f.config.state.control_socket)
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&result.stdout).unwrap()["target"],
        "/status"
    );
    rejected(
        f.client(Some("a".repeat(64)))
            .request("GET", "/status", None)
            .await
            .unwrap_err(),
        401,
    );
    server.close().await.unwrap();
    assert!(!f.config.state.control_socket.exists());
    let readonly = Capability::generate(Authority::DesktopReadOnly).unwrap();
    let owner = Capability::generate(Authority::Owner).unwrap();
    let overseer = Capability::generate(Authority::Overseer).unwrap();
    let tokens = [
        readonly.secret().to_string(),
        owner.secret().to_string(),
        overseer.secret().to_string(),
    ];
    let access = Access::capabilities(&[readonly, owner, overseer]).unwrap();
    let server = f.bind(echo.clone(), access, Options::default()).await;
    let token_file = f._dir.path().join("capability");
    std::fs::write(&token_file, &tokens[1]).unwrap();
    std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    for mode in [0o600, 0o644] {
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(mode)).unwrap();
        let result = tokio::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args(["status", "--socket"])
            .arg(&f.config.state.control_socket)
            .arg("--capability-file")
            .arg(&token_file)
            .output()
            .await
            .unwrap();
        assert_eq!(result.status.success(), mode == 0o600);
        assert!(!String::from_utf8_lossy(&result.stdout).contains(&tokens[1]));
        assert!(!String::from_utf8_lossy(&result.stderr).contains(&tokens[1]));
    }
    rejected(
        f.client(None)
            .request("GET", "/status", None)
            .await
            .unwrap_err(),
        401,
    );
    assert_eq!(
        f.client(Some(tokens[0].clone()))
            .request("GET", "/status", None)
            .await
            .unwrap()["authority"]["kind"],
        "desktop_read_only"
    );
    let before = echo.calls.load(Ordering::SeqCst);
    rejected(
        f.client(Some(tokens[0].clone()))
            .request("POST", "/threads/t/resume", Some(json!({"actor":"owner"})))
            .await
            .unwrap_err(),
        403,
    );
    assert_eq!(echo.calls.load(Ordering::SeqCst), before);
    assert_eq!(
        f.client(Some(tokens[2].clone()))
            .request("POST", "/threads/t/resume", Some(json!({"actor":"owner"})))
            .await
            .unwrap()["authority"]["kind"],
        "overseer"
    );
    let result=f.raw(&format!("GET /status HTTP/1.1\r\nHost: fridica\r\nAuthorization: Bearer {}\r\nAuthorization: Bearer {}\r\n\r\n",tokens[1],tokens[2]),b"").await;
    assert!(result.starts_with("HTTP/1.1 401"));
    for token in tokens {
        assert!(!result.contains(&token));
    }
    server.close().await.unwrap();
}
#[tokio::test]
async fn stale_socket_recovery_refuses_unsafe_paths_and_preserves_replacements() {
    let f = Fixture::new();
    let e = Arc::new(Echo::default());
    let path = &f.config.state.control_socket;
    let parent = path.parent().unwrap();
    std::fs::create_dir(parent).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(path, b"do not delete").unwrap();
    assert!(matches!(
        Server::bind(&f.config, e.clone(), Access::OwnerPeer, Options::default()).await,
        Err(Failure::UnsafePath)
    ));
    assert_eq!(std::fs::read(path).unwrap(), b"do not delete");
    std::fs::remove_file(path).unwrap();
    symlink(f._dir.path().join("db"), path).unwrap();
    assert!(matches!(
        Server::bind(&f.config, e.clone(), Access::OwnerPeer, Options::default()).await,
        Err(Failure::UnsafePath)
    ));
    std::fs::remove_file(path).unwrap();
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(matches!(
        Server::bind(&f.config, e.clone(), Access::OwnerPeer, Options::default()).await,
        Err(Failure::UnsafePath)
    ));
    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let legacy = std::os::unix::net::UnixListener::bind(path).unwrap();
    assert!(matches!(
        Server::bind(&f.config, e.clone(), Access::OwnerPeer, Options::default()).await,
        Err(Failure::AlreadyRunning)
    ));
    assert!(path.exists());
    drop(legacy);
    let server = f
        .bind(e.clone(), Access::OwnerPeer, Options::default())
        .await;
    assert!(matches!(
        Server::bind(&f.config, e, Access::OwnerPeer, Options::default()).await,
        Err(Failure::AlreadyRunning)
    ));
    f.client(None)
        .request("GET", "/status", None)
        .await
        .unwrap();
    std::fs::remove_file(path).unwrap();
    std::fs::write(path, b"replacement").unwrap();
    server.close().await.unwrap();
    assert_eq!(std::fs::read(path).unwrap(), b"replacement");
}
struct Blocked {
    entered: Semaphore,
    release: Semaphore,
    finished: AtomicUsize,
}
impl Backend for Blocked {
    fn request(&self, _: Request, _: Authority) -> AdapterFuture<'_, Response> {
        Box::pin(async {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            self.finished.fetch_add(1, Ordering::SeqCst);
            Response::ok(json!({"committed":true}))
        })
    }
}
#[tokio::test]
async fn disconnected_accepted_operation_survives_and_shutdown_drains_it() {
    let f = Fixture::new();
    let b = Arc::new(Blocked {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        finished: AtomicUsize::new(0),
    });
    let server = f
        .bind(
            b.clone(),
            Access::OwnerPeer,
            Options {
                operations: 1,
                ..Options::default()
            },
        )
        .await;
    let mut s = UnixStream::connect(&f.config.state.control_socket)
        .await
        .unwrap();
    s.write_all(b"POST /threads/t/close HTTP/1.1\r\nHost: fridica\r\nContent-Length: 2\r\n\r\n{}")
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), b.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    drop(s);
    rejected(
        f.client(None)
            .request("GET", "/status", None)
            .await
            .unwrap_err(),
        503,
    );
    let closing = tokio::spawn(server.close());
    tokio::task::yield_now().await;
    assert!(!closing.is_finished());
    b.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(b.finished.load(Ordering::SeqCst), 1);
    assert!(!f.config.state.control_socket.exists());
}
#[tokio::test]
async fn malformed_oversized_and_slow_bodies_never_reach_backend() {
    let f = Fixture::new();
    let e = Arc::new(Echo::default());
    let server = f
        .bind(
            e.clone(),
            Access::OwnerPeer,
            Options {
                body_timeout: Duration::from_millis(100),
                ..Options::default()
            },
        )
        .await;
    for (headers,body,status) in [
        ("POST /status HTTP/1.1\r\nHost: fridica\r\nContent-Length: 2\r\n\r\n".to_string(),b"[]".to_vec(),400),
        ("GET /status HTTP/1.1\r\nHost: fridica\r\nOrigin: http://localhost\r\n\r\n".into(),vec![],400),
        ("POST /status HTTP/1.1\r\nHost: fridica\r\nContent-Length: 20\r\n\r\n".into(),b"{}".to_vec(),408),
        (format!("POST /status HTTP/1.1\r\nHost: fridica\r\nContent-Length: {}\r\n\r\n",BODY_LIMIT+1),vec![b'x';BODY_LIMIT+1],413),
        ("POST /status HTTP/1.1\r\nHost: fridica\r\nContent-Length: 2\r\nContent-Length: 3\r\n\r\n".into(),b"{}".to_vec(),400),
    ]{let response=f.raw(&headers,&body).await;assert!(response.starts_with(&format!("HTTP/1.1 {status}")),"{response}");}
    assert_eq!(e.calls.load(Ordering::SeqCst), 0);
    server.close().await.unwrap();
}
#[test]
fn targets_refuse_normalization_and_query_ambiguity() {
    for bad in [
        "/threads/../status",
        "/threads/%2e%2e/status",
        "/threads/.",
        "/threads/%2fstatus",
        "/threads/x\\y",
        "//status",
        "/status#fragment",
        "/status?limit=0",
        "/status?limit=1001",
        "/status?limit=2&limit=3",
        "/status?unknown=1",
        "/status/%00",
        "/status/%ff",
        "/status/%",
    ] {
        assert!(target(bad).is_none(), "{bad}");
    }
    assert_eq!(
        target("/threads/TTEAM%3ACROOM%3A100.1?limit=2").unwrap().0[1],
        "TTEAM:CROOM:100.1"
    );
}

#[test]
fn frozen_python_control_views_preserve_legacy_fields_and_attention_selection() {
    use fridica::{control::views, store::schema};
    use serde_json::Value;
    fn legacy(actual: &Value, expected: &Value, path: &str) {
        match expected {
            Value::Object(fields) => {
                assert!(actual.is_object(), "{path}: {actual}");
                for (k, v) in fields {
                    assert!(actual.get(k).is_some(), "{path}/{k} missing");
                    legacy(&actual[k], v, &format!("{path}/{k}"));
                }
            }
            Value::Array(items) => {
                let a = actual
                    .as_array()
                    .unwrap_or_else(|| panic!("{path}: {actual}"));
                assert_eq!(a.len(), items.len(), "{path}");
                for (i, (a, e)) in a.iter().zip(items).enumerate() {
                    legacy(a, e, &format!("{path}/{i}"));
                }
            }
            _ => assert_eq!(actual, expected, "{path}"),
        }
    }
    let corpus: Value = serde_json::from_str(include_str!("corpus/control.json")).unwrap();
    let mut db = rusqlite::Connection::open_in_memory().unwrap();
    schema::migrate(&mut db).unwrap();
    for seed in corpus["seed"].as_array().unwrap() {
        let params = seed[1].as_array().unwrap().iter().map(|v| match v {
            Value::Null => rusqlite::types::Value::Null,
            Value::String(s) => s.clone().into(),
            Value::Number(n) if n.is_i64() => n.as_i64().unwrap().into(),
            Value::Number(n) => n.as_f64().unwrap().into(),
            _ => panic!("invalid fixture parameter"),
        });
        db.execute(
            seed[0].as_str().unwrap(),
            rusqlite::params_from_iter(params),
        )
        .unwrap();
    }
    let f = Fixture::new();
    let processes = std::collections::BTreeMap::from([("worker-1".into(), "busy".into())]);
    for case in corpus["cases"].as_array().unwrap() {
        let path = case["target"].as_str().unwrap();
        let (parts, query) = target(path).unwrap();
        let actual = views::get(&db, &f.config, &parts, &query, &processes, false)
            .unwrap()
            .unwrap();
        legacy(&actual, &case["expected"], path);
    }
    db.execute("UPDATE jobs SET status='done' WHERE id='job-2'", []).unwrap();
    let (parts, query) = target("/jobs?status=all&limit=4").unwrap();
    let recent = views::get(&db, &f.config, &parts, &query, &processes, false)
        .unwrap().unwrap();
    assert_eq!(recent.as_array().unwrap().len(), 4);
    assert_eq!(recent[3]["id"], "job-2");
}

#[tokio::test]
async fn shutdown_deadline_reports_incomplete_work_and_removes_socket() {
    let f = Fixture::new();
    let b = Arc::new(Blocked {
        entered: Semaphore::new(0),
        release: Semaphore::new(0),
        finished: AtomicUsize::new(0),
    });
    let server = f
        .bind(
            b.clone(),
            Access::OwnerPeer,
            Options {
                shutdown_timeout: Duration::from_millis(30),
                ..Options::default()
            },
        )
        .await;
    let mut stream = UnixStream::connect(&f.config.state.control_socket)
        .await
        .unwrap();
    stream
        .write_all(
            b"POST /threads/t/close HTTP/1.1\r\nHost: fridica\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), b.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), server.close())
            .await
            .unwrap(),
        Err(Failure::Task)
    );
    assert_eq!(b.finished.load(Ordering::SeqCst), 0);
    assert!(!f.config.state.control_socket.exists());
}

#[tokio::test]
async fn obligations_cli_sends_bounded_explicit_backfill_or_owner_close_requests() {
    let f = Fixture::new();
    let echo = Arc::new(Echo::default());
    let server = f
        .bind(echo.clone(), Access::OwnerPeer, Options::default())
        .await;
    for (args, target, body) in [
        (vec!["obligations"], "/obligations", json!({})),
        (
            vec![
                "obligations",
                "--backfill",
                "--since",
                "100",
                "--until",
                "200",
            ],
            "/obligations/backfill",
            json!({"since":100.,"until":200.,"apply":false,"client_id":""}),
        ),
        (
            vec![
                "obligations",
                "--backfill",
                "--since",
                "100",
                "--until",
                "200",
                "--apply",
                "--client-id",
                "backfill-1234",
            ],
            "/obligations/backfill",
            json!({"since":100.,"until":200.,"apply":true,"client_id":"backfill-1234"}),
        ),
        (
            vec![
                "obligations",
                "--close",
                "o1",
                "--reason",
                "Already handled",
            ],
            "/obligations/o1/close",
            json!({"reason":"Already handled"}),
        ),
    ] {
        let mut argv = vec![env!("CARGO_BIN_EXE_fridica").to_string()];
        argv.extend(args.into_iter().map(str::to_owned));
        argv.extend([
            "--socket".into(),
            f.config.state.control_socket.to_str().unwrap().into(),
        ]);
        let result = fridica::exec::process::run_once(
            fridica::exec::process::Launch {
                argv,
                cwd: None,
                env: std::collections::BTreeMap::new(),
            },
            vec![],
            Duration::from_secs(5),
            4096,
        )
        .await
        .unwrap();
        assert_eq!(
            result.returncode,
            0,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let output: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(output["target"], target);
        assert_eq!(output["body"], body);
    }
    let before = echo.calls.load(Ordering::SeqCst);
    for args in [
        vec!["obligations", "--apply"],
        vec!["obligations", "--backfill"],
        vec![
            "obligations",
            "--backfill",
            "--since",
            "NaN",
            "--until",
            "200",
        ],
        vec!["obligations", "--close", "o1"],
    ] {
        let mut argv = vec![env!("CARGO_BIN_EXE_fridica").to_string()];
        argv.extend(args.into_iter().map(str::to_owned));
        argv.extend([
            "--socket".into(),
            f.config.state.control_socket.to_str().unwrap().into(),
        ]);
        let result = fridica::exec::process::run_once(
            fridica::exec::process::Launch {
                argv,
                cwd: None,
                env: std::collections::BTreeMap::new(),
            },
            vec![],
            Duration::from_secs(5),
            4096,
        )
        .await
        .unwrap();
        assert_ne!(result.returncode, 0);
    }
    assert_eq!(echo.calls.load(Ordering::SeqCst), before);
    server.close().await.unwrap();
}

async fn bounded_cli(args: Vec<String>) -> fridica::exec::process::Completed {
    let mut argv = vec![env!("CARGO_BIN_EXE_fridica").into()];
    argv.extend(args);
    fridica::exec::process::run_once(
        fridica::exec::process::Launch {
            argv,
            cwd: None,
            env: std::collections::BTreeMap::new(),
        },
        vec![],
        Duration::from_secs(5),
        32 * 1024,
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn frozen_cli_commands_preserve_requests_and_authenticated_identity() {
    let f = Fixture::new();
    let echo = Arc::new(Echo::default());
    let server = f
        .bind(echo.clone(), Access::OwnerPeer, Options::default())
        .await;
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("corpus/control_cli.json")).unwrap();
    for command in corpus["commands"].as_array().unwrap() {
        let name = command.as_str().unwrap();
        let help = bounded_cli(vec![name.into(), "--help".into()]).await;
        assert_eq!(
            help.returncode,
            if name == "dashboard" { 2 } else { 0 },
            "{name}"
        );
    }
    for case in corpus["cases"].as_array().unwrap() {
        let mut args: Vec<String> = case["args"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().into())
            .collect();
        args.extend([
            "--socket".into(),
            f.config.state.control_socket.to_str().unwrap().into(),
        ]);
        let output = bounded_cli(args).await;
        assert_eq!(
            output.returncode,
            case["exit"].as_i64().unwrap() as i32,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let actual: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let mut expected = case["request"].clone();
        // The body cannot confer authority. Authentication replaces Python's actor hint.
        expected["body"].as_object_mut().unwrap().remove("actor");
        for key in ["method", "target", "body"] {
            assert_eq!(actual[key], expected[key], "{}: {key}", case["args"]);
        }
        assert_eq!(actual["authority"]["kind"], "owner");
    }
    assert_eq!(echo.calls.load(Ordering::SeqCst), 21);
    assert!(!f.config.state.path.exists());
    server.close().await.unwrap();
}

struct Reject;
impl Backend for Reject {
    fn request(&self, _: Request, _: Authority) -> AdapterFuture<'_, Response> {
        Box::pin(async { Response::error(409, "operation_conflict") })
    }
}

#[tokio::test]
async fn cli_exit_codes_distinguish_input_unavailable_and_rejected_controls() {
    let f = Fixture::new();
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("corpus/control_cli.json")).unwrap();
    let code = |kind| {
        corpus["errors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == kind)
            .unwrap()["exit"]
            .as_i64()
            .unwrap() as i32
    };
    let args = |values: &[&str]| {
        let mut args: Vec<String> = values.iter().map(|v| (*v).into()).collect();
        args.extend([
            "--socket".into(),
            f.config.state.control_socket.to_str().unwrap().into(),
        ]);
        args
    };
    let missing = bounded_cli(args(&["status"])).await;
    assert_eq!(missing.returncode, code("DaemonUnavailable"));
    assert!(missing.stdout.is_empty());
    let server = f
        .bind(Arc::new(Reject), Access::OwnerPeer, Options::default())
        .await;
    let rejected = bounded_cli(args(&["approvals", "approval-1", "once"])).await;
    assert_eq!(rejected.returncode, code("ControlError"));
    assert!(rejected.stdout.is_empty());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("operation_conflict"));
    for invalid in [
        vec!["threads", "../other", "resume"],
        vec!["outbox", "0"],
        vec!["threads", "t", "unknown"],
    ] {
        let result = bounded_cli(args(&invalid)).await;
        assert_eq!(result.returncode, code("ValueError"));
        assert!(result.stdout.is_empty());
    }
    let bad = f._dir.path().join("bad.toml");
    std::fs::write(&bad, "[owner]\n").unwrap();
    for command in ["status", "start"] {
        let mut argv = vec![
            command.into(),
            "--config".into(),
            bad.to_str().unwrap().into(),
        ];
        if command == "start" {
            argv.push("--observe-only".into());
        }
        assert_eq!(bounded_cli(argv).await.returncode, code("ValueError"));
    }
    assert!(!f.config.state.path.exists());
    server.close().await.unwrap();
}

#[tokio::test]
async fn cli_lost_or_invalid_responses_never_retry_or_print_private_content() {
    for malformed in [false, true] {
        let f = Fixture::new();
        std::fs::create_dir_all(f.config.state.control_socket.parent().unwrap()).unwrap();
        let listener = tokio::net::UnixListener::bind(&f.config.state.control_socket).unwrap();
        std::fs::set_permissions(
            &f.config.state.control_socket,
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            if malformed {
                stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 14\r\nConnection: close\r\n\r\nprivate-secret").await.unwrap();
            }
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "client retried an uncertain mutation"
            );
        });
        let out = bounded_cli(vec![
            "threads".into(),
            "thread".into(),
            "resume".into(),
            "--socket".into(),
            f.config.state.control_socket.to_str().unwrap().into(),
        ])
        .await;
        assert_eq!(out.returncode, 3);
        assert!(out.stdout.is_empty());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-secret"));
        server.await.unwrap();
    }
}
