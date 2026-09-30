use fridica::control::client::Client;
use rusqlite::Connection;
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{io::AsyncReadExt, net::TcpListener, process::Command};

struct Fixture {
    dir: tempfile::TempDir,
    config: PathBuf,
    db: PathBuf,
    socket: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("project")).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local.workspaces]
project="project"
[state]
path="db"
control_socket="private/control.sock"
"#,
        )
        .unwrap();
        let db = dir.path().join("db");
        let socket = dir.path().join("private/control.sock");
        Self {
            dir,
            config,
            db,
            socket,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_fridica"));
        command
            .args(["start", "--config"])
            .arg(&self.config)
            .env_clear()
            .env("HOME", self.dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }
    fn observer(&self) -> Command {
        let mut command = self.command();
        command
            .arg("--observe-only")
            .env("SLACK_APP_TOKEN", "xapp-synthetic")
            .env("SLACK_USER_TOKEN", "xoxp-synthetic");
        command
    }
}

#[tokio::test]
async fn active_launch_and_invalid_tokens_fail_before_state_creation() {
    let f = Fixture::new();
    // A bare start is the active daemon; without credentials it stops before state.
    let output = f.command().output().await.unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("SLACK_"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!f.db.exists());
    let output = f
        .observer()
        .env("SLACK_USER_TOKEN", "xoxp-private\nsecret")
        .output()
        .await
        .unwrap();
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(error.contains("SLACK_USER_TOKEN"));
    assert!(!error.contains("private"));
    assert!(!error.contains("secret"));
    assert!(!f.db.exists());
    assert!(!f.socket.exists());
}

#[tokio::test]
async fn observer_refuses_legacy_schema_without_migrating_or_opening_control() {
    let f = Fixture::new();
    let db = Connection::open(&f.db).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT); INSERT INTO meta VALUES('schema_version','5');").unwrap();
    let before = std::fs::read(&f.db).unwrap();
    let output = f.observer().output().await.unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("schema v5"));
    assert_eq!(std::fs::read(&f.db).unwrap(), before);
    assert!(!f.socket.exists());
}

#[tokio::test]
async fn signals_drain_real_observer_and_database_lock_rejects_second_launch() {
    use rustix::process::{kill_process, Pid, Signal};
    for signal in [Signal::TERM, Signal::INT] {
        let f = Fixture::new();
        // The production HTTPS client reaches only this local proxy. Hold its
        // CONNECT request without tunneling, so no live Slack connection occurs.
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", proxy.local_addr().unwrap());
        let child = f
            .observer()
            .env("HTTPS_PROXY", &url)
            .env("NO_PROXY", "")
            .spawn()
            .unwrap();
        let pid = Pid::from_raw(child.id().unwrap() as i32).unwrap();
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), proxy.accept())
            .await
            .unwrap()
            .unwrap();
        let mut request = [0; 1024];
        let count = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut request))
            .await
            .unwrap()
            .unwrap();
        let request = String::from_utf8_lossy(&request[..count]);
        assert!(request.starts_with("CONNECT slack.com:443 "));
        assert!(!request.contains("synthetic"));
        let client = Client::new(&f.socket, None).unwrap();
        assert_eq!(
            client.request("GET", "/status", None).await.unwrap()["observe_only"],
            true
        );
        let duplicate = f
            .observer()
            .env("HTTPS_PROXY", &url)
            .output()
            .await
            .unwrap();
        assert!(!duplicate.status.success());
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("another Fridica process"));
        kill_process(pid, signal).unwrap();
        let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!f.socket.exists());
        let db = Connection::open(&f.db).unwrap();
        let stopped: bool = db.query_row("SELECT slack_status='stopped' AND control_socket='' AND observe_only=1 FROM runtime", [], |r| r.get(0)).unwrap();
        assert!(stopped);
        assert_eq!(
            db.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT count(*) FROM outbox", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM replay_events WHERE kind='service_stop'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }
}
