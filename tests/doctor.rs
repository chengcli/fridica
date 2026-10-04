use fridica::{
    config::LoadContext,
    doctor::checks::{self, Report, Status},
    exec::process,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::sync::watch;

const BACKEND: &str = r#"#!/usr/bin/python3
import json, os, sys, time
from pathlib import Path
name = Path(sys.argv[0]).name
args = sys.argv[1:]
assert not any('SLACK' in k or k.startswith('FRIDICA_') or k == 'SECRET_GITHUB' for k in os.environ)
with open(os.environ['CALLS'], 'a') as stream:
    stream.write(json.dumps([name,args])+'\n')
mode = os.environ.get('MODE','')
modes = set(mode.split(','))
if mode == 'flood':
    print('private-secret-' * 400000)
    sys.exit(1)
if name == 'claude':
    if args == ['--help']:
        print('--input-format --permission-prompts --json-schema --setting-sources --strict-mcp-config --append-system-prompt --session-id dontAsk' + (' "auto"' if 'noauto' not in modes else ''))
        sys.exit(0 if 'badexit' not in modes else 1)
    if args == ['auth','status']:
        print(json.dumps({'loggedIn':'logout' not in modes}))
        sys.exit(0)
if name == 'codex':
    if args == ['exec','--help']:
        print('--ignore-user-config --ignore-rules --output-schema --ephemeral'); sys.exit(0)
    if args == ['login','status']:
        sys.exit(1 if 'logout' in modes else 0)
    if args[:2] == ['app-server','generate-json-schema']:
        out = Path(args[3])
        if mode == 'hang':
            child = os.fork()
            if child == 0:
                while True: time.sleep(1)
            Path(os.environ['OUT']).write_text(str(out))
            Path(os.environ['PID']).write_text(str(child))
            while True: time.sleep(1)
        if mode == 'symlink':
            (out / 'schema.json').symlink_to(os.environ['SECRET_FILE'])
        else:
            (out / 'schema.json').write_text('turn/interrupt item/commandExecution/requestApproval outputSchema' + (' auto_review' if 'noauto' not in modes else ''))
        sys.exit(0)
if name in ('bwrap','socat'): sys.exit(0)
sys.exit(92)
"#;
/// A `gh` that answers the reader probe: authenticated when a token reached
/// it, exit 4 (gh's "not logged in") otherwise, down when asked.
const GH: &str = r#"#!/usr/bin/python3
import os, sys
assert not any('SLACK' in k or k == 'SECRET_GITHUB' for k in os.environ)
assert sys.argv[1:4] == ['api', '--hostname', 'github.com'] and sys.argv[-1] == '/rate_limit'
if os.path.exists(os.path.join(os.environ['HOME'], 'gh-down')):
    sys.exit(1)
if 'GH_TOKEN' not in os.environ:
    sys.stderr.write('To get started with GitHub CLI, please run:  gh auth login\n')
    sys.exit(4)
sys.stdout.write('HTTP/2.0 200 OK\r\nContent-Type: application/json\r\n\r\n{"resources":{"core":{"limit":5000}}}')
"#;
struct Fixture {
    dir: tempfile::TempDir,
    path: PathBuf,
    context: LoadContext,
    env: BTreeMap<OsString, OsString>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["home", "bin", "project"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let path = root.join("config.toml");
        std::fs::write(
            &path,
            format!(
                r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[parent]
backend="claude"
[machines.local]
backends=["codex","claude"]
[machines.local.policy]
gpu_confine=false
[machines.local.workspaces]
project={}
[state]
path="state.db"
control_socket="control.sock"
[github]
token_env="SECRET_GITHUB"
"#,
                json!(root.join("project"))
            ),
        )
        .unwrap();
        let context = LoadContext {
            home: root.join("home"),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        };
        let env = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("HOME".into(), root.join("home").into_os_string()),
            ("CALLS".into(), root.join("calls").into_os_string()),
            ("PID".into(), root.join("pid").into_os_string()),
            ("OUT".into(), root.join("out").into_os_string()),
            ("SLACK_APP_TOKEN".into(), "xapp-private-secret".into()),
            ("SLACK_USER_TOKEN".into(), "xoxp-private-secret".into()),
            ("SECRET_GITHUB".into(), "private-secret".into()),
        ]);
        let fixture = Self {
            dir,
            path,
            context,
            env,
        };
        for name in ["claude", "codex", "bwrap", "socat"] {
            fixture.executable(name, BACKEND);
        }
        fixture.executable("gh", GH);
        fixture
    }
    fn executable(&self, name: &str, source: &str) {
        let path = self.dir.path().join("bin").join(name);
        std::fs::write(&path, source).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    fn append(&self, source: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&self.path)
            .unwrap()
            .write_all(source.as_bytes())
            .unwrap();
    }
    async fn run(&self) -> Report {
        checks::run(
            &self.path,
            &self.context,
            self.env.clone(),
            Duration::from_secs(2),
            watch::channel(false).1,
        )
        .await
        .unwrap()
    }
    fn pristine(&self) {
        for name in [
            "state.db",
            "state.db-wal",
            "state.db-shm",
            "control.sock",
            "home/.codex",
            "project/worker1",
        ] {
            assert!(!self.dir.path().join(name).exists(), "created {name}");
        }
    }
    async fn cli(&self, args: &[&str]) -> process::Completed {
        process::run_once(
            process::Launch {
                argv: std::iter::once(env!("CARGO_BIN_EXE_fridica").into())
                    .chain(args.iter().map(|s| s.to_string()))
                    .chain(["--config".into(), self.path.to_str().unwrap().into()])
                    .collect(),
                env: self.env.clone(),
                cwd: None,
            },
            vec![],
            Duration::from_secs(20),
            32768,
        )
        .await
        .unwrap()
    }
}
fn status(report: &Report, name: &str) -> Status {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("{name}: {}", report.text()))
        .status
}

#[tokio::test]
async fn checks_all_targets_without_model_requests_or_state_creation() {
    let fixture = Fixture::new();
    fixture.append(&format!(
        r#"
[machines.remote]
host="fixture"
backends=["codex"]
[machines.remote.workspaces]
project={}
missing="/definitely/not/a/workspace"
[machines.batch]
transport="slurm"
host="fixture"
slurm={{partition="gpu"}}
[machines.batch.workspaces]
project="/scratch"
"#,
        json!(fixture.dir.path().join("project"))
    ));
    fixture.executable(
        "ssh",
        r#"#!/usr/bin/python3
import os,sys
assert 'BatchMode=yes' in sys.argv
assert 'StrictHostKeyChecking=yes' in sys.argv
assert 'ControlMaster=auto' in sys.argv and sys.argv.index('ControlPersist=30') < sys.argv.index('ControlPersist=600')
assert 'UpdateHostKeys=no' in sys.argv
assert '-A' not in sys.argv
# Pretend the remote login environment also contains daemon tokens.
os.environ['SLACK_USER_TOKEN']='xoxp-private-secret'
os.environ['SECRET_GITHUB']='private-secret'
os.execv('/bin/sh',['sh','-c',sys.argv[-1]])
"#,
    );
    let report = fixture.run().await;
    assert_eq!(status(&report, "Machine remote"), Status::Pass);
    assert_eq!(
        status(&report, "Machine remote workspace project"),
        Status::Pass
    );
    assert_eq!(
        status(&report, "Machine remote workspace missing"),
        Status::Fail
    );
    assert_eq!(status(&report, "codex (worker on remote)"), Status::Pass);
    assert_eq!(status(&report, "Machine batch"), Status::Skip);
    assert!(!report.passed());
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("private-secret"));
    assert!(!text.contains(fixture.dir.path().to_str().unwrap()));
    fixture.pristine();
}

#[tokio::test]
async fn github_reader_is_probed_without_recording_and_warns_when_it_cannot_read() {
    let fixture = Fixture::new();
    assert_eq!(status(&fixture.run().await, "GitHub reader"), Status::Pass);
    // No token reaches gh: it is not logged in, so repository context is off.
    let mut unauthenticated = Fixture::new();
    unauthenticated.env.remove(&OsString::from("SECRET_GITHUB"));
    let report = unauthenticated.run().await;
    assert_eq!(status(&report, "GitHub reader"), Status::Warn);
    let check = report
        .checks
        .iter()
        .find(|c| c.name == "GitHub reader")
        .unwrap();
    assert!(check.detail.contains("gh auth status"), "{}", check.detail);
    assert!(report.passed(), "{}", report.text());
    // gh itself fails (not installed, no network): still only a warning.
    let down = Fixture::new();
    std::fs::create_dir_all(down.dir.path().join("home")).unwrap();
    std::fs::write(down.dir.path().join("home/gh-down"), "").unwrap();
    assert_eq!(status(&down.run().await, "GitHub reader"), Status::Warn);
    let disabled = Fixture::new();
    let config = std::fs::read_to_string(&disabled.path).unwrap();
    std::fs::write(
        &disabled.path,
        config.replace("[github]\n", "[github]\nenabled=false\n"),
    )
    .unwrap();
    assert_eq!(status(&disabled.run().await, "GitHub reader"), Status::Skip);
    fixture.pristine();
}

#[tokio::test]
async fn auth_capability_exit_status_and_auto_requirements_are_checked() {
    let mut fixture = Fixture::new();
    assert!(fixture.run().await.passed());
    for (mode, expected, detail) in [
        ("logout", "claude (parent locally)", "not signed in"),
        ("noauto", "codex (worker on local)", "auto_review"),
        ("badexit", "claude (parent locally)", "protocol"),
    ] {
        fixture.env.insert("MODE".into(), mode.into());
        let report = fixture.run().await;
        assert_eq!(status(&report, expected), Status::Fail);
        assert!(report.text().contains(detail));
        if mode == "noauto" {
            assert_eq!(status(&report, "claude (parent locally)"), Status::Pass);
            assert!(report.text().contains("--permission-mode auto"));
        }
    }
    // Shortcomings are independent facets: a backend missing a protocol flag
    // and signed out shows both on its one line.
    fixture.env.insert("MODE".into(), "badexit,logout".into());
    let report = fixture.run().await;
    let line = report
        .checks
        .iter()
        .find(|c| c.name == "claude (parent locally)")
        .unwrap();
    assert_eq!(line.status, Status::Fail);
    assert!(
        line.detail.contains("protocol") && line.detail.contains("not signed in"),
        "{}",
        line.detail
    );
    fixture.pristine();
}

#[tokio::test]
async fn malformed_auth_missing_backend_and_failed_ssh_have_fixed_diagnostics() {
    let fixture = Fixture::new();
    fixture.executable(
        "claude",
        "#!/bin/sh\nprintf 'private-secret malformed auth'\n",
    );
    std::fs::remove_file(fixture.dir.path().join("bin/codex")).unwrap();
    fixture.append(
        "\n[machines.remote]\nhost='fixture'\n[machines.remote.workspaces]\nproject='/x'\n",
    );
    fixture.executable("ssh", "#!/bin/sh\nprintf 'private-secret' >&2\nexit 255\n");
    let report = fixture.run().await;
    assert_eq!(status(&report, "claude (parent locally)"), Status::Fail);
    assert_eq!(status(&report, "codex (worker on local)"), Status::Fail);
    assert_eq!(status(&report, "Machine remote"), Status::Fail);
    assert!(report.text().contains("without a prompt"));
    assert!(!report.text().contains("private-secret"));
    fixture.pristine();
}

#[tokio::test]
async fn schema_reads_reject_symlinks_and_backend_output_is_bounded() {
    let mut fixture = Fixture::new();
    let secret = fixture.dir.path().join("secret");
    std::fs::write(&secret,"turn/interrupt item/commandExecution/requestApproval outputSchema auto_review private-secret").unwrap();
    fixture
        .env
        .insert("SECRET_FILE".into(), secret.into_os_string());
    for mode in ["symlink", "flood"] {
        fixture.env.insert("MODE".into(), mode.into());
        let report = fixture.run().await;
        assert!(!report.passed());
        assert_eq!(status(&report, "codex (worker on local)"), Status::Fail);
        assert!(!report.text().contains("private-secret"));
    }
    fixture.pristine();
}

#[tokio::test]
async fn timeout_reaps_descendants_and_removes_schema_scratch_directory() {
    let mut fixture = Fixture::new();
    fixture.env.insert("MODE".into(), "hang".into());
    let report = fixture.run().await;
    assert_eq!(status(&report, "codex (worker on local)"), Status::Fail);
    let scratch = std::fs::read_to_string(fixture.dir.path().join("out")).unwrap();
    assert!(!Path::new(&scratch).exists());
    let pid = std::fs::read_to_string(fixture.dir.path().join("pid")).unwrap();
    // A terminated orphan may briefly remain a zombie until the host reaper runs.
    #[cfg(target_os = "linux")]
    if let Ok(state) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        assert_eq!(
            state.split(')').nth(1).unwrap().split_whitespace().next(),
            Some("Z")
        );
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid.trim()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&out.stdout);
        assert!(state.trim().is_empty() || state.trim().starts_with('Z'));
    }
    fixture.pristine();
}

#[tokio::test]
async fn stopping_finishes_current_probe_and_skips_later_targets() {
    let mut fixture = Fixture::new();
    fixture.env.insert("MODE".into(), "hang".into());
    let (stop, rx) = watch::channel(false);
    let run = checks::run(
        &fixture.path,
        &fixture.context,
        fixture.env.clone(),
        Duration::from_secs(2),
        rx,
    );
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(20), async {
            while !fixture.dir.path().join("pid").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        stop.send_replace(true);
    };
    let (report, ()) = tokio::join!(run, cancel);
    let report = report.unwrap();
    assert!(report.cancelled);
    assert!(!report.passed());
    assert!(!report
        .checks
        .iter()
        .any(|c| c.name == "claude (worker on local)"));
    let scratch = std::fs::read_to_string(fixture.dir.path().join("out")).unwrap();
    assert!(!Path::new(&scratch).exists());
    fixture.pristine();
}

#[tokio::test]
async fn recorded_attachment_scopes_are_read_without_migration() {
    let fixture = Fixture::new();
    assert_eq!(
        status(&fixture.run().await, "Slack files:read"),
        Status::Skip
    );
    fixture.pristine();
    let db = rusqlite::Connection::open(fixture.dir.path().join("state.db")).unwrap();
    db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO meta VALUES('slack_scopes','chat:write'); PRAGMA user_version=4;").unwrap();
    assert_eq!(
        status(&fixture.run().await, "Slack files:read"),
        Status::Warn
    );
    db.execute("UPDATE meta SET value='chat:write,files:read'", [])
        .unwrap();
    assert_eq!(
        status(&fixture.run().await, "Slack files:read"),
        Status::Pass
    );
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
}

#[tokio::test]
async fn cli_reports_json_and_text_and_preserves_active_launch_gate() {
    let mut fixture = Fixture::new();
    let result = fixture.cli(&["doctor", "--json", "--timeout", "2"]).await;
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let report: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert!(report.get("active_launch_ready").is_none());
    let result = fixture.cli(&["doctor", "--timeout", "2"]).await;
    assert_eq!(result.returncode, 0);
    assert!(result.text().contains("0 failed"));
    fixture.env.remove(std::ffi::OsStr::new("SLACK_APP_TOKEN"));
    let result = fixture.cli(&["doctor", "--timeout", "2"]).await;
    assert_eq!(result.returncode, 1);
    assert!(result.text().contains("FAIL Slack app token"));
    assert_ne!(fixture.cli(&["start"]).await.returncode, 0);
    fixture.pristine();
    std::fs::write(&fixture.path, "private-secret invalid TOML").unwrap();
    let result = fixture.cli(&["doctor", "--json"]).await;
    assert_eq!(result.returncode, 1);
    assert!(!result.text().contains("private-secret"));
    let value: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(value["checks"][1]["status"], "FAIL");
    assert_eq!(value["checks"][2]["status"], "SKIP");
}

#[tokio::test]
async fn codex_parent_owner_inputs_and_sandbox_failures_are_independent() {
    let mut fixture = Fixture::new();
    for name in ["invalid-repos.toml", "invalid-contract.md"] {
        std::fs::write(
            fixture.dir.path().join(name),
            "private-secret invalid content",
        )
        .unwrap();
    }
    let source = std::fs::read_to_string(&fixture.path)
        .unwrap()
        .replace(
            "backend=\"claude\"",
            "backend=\"codex\"\nrepos=\"invalid-repos.toml\"",
        )
        .replace(
            "slack_user=\"UOWNER\"",
            "slack_user=\"UOWNER\"\ncontract=\"invalid-contract.md\"",
        )
        .replace(
            "gpu_confine=false",
            "gpu_confine=false\napprovals=\"never\"",
        );
    std::fs::write(&fixture.path, source).unwrap();
    fixture.env.insert("MODE".into(), "noauto".into());
    fixture.executable("bwrap", "#!/bin/sh\nprintf 'private-secret' >&2\nexit 1\n");
    let report = fixture.run().await;
    assert_eq!(status(&report, "codex (parent locally)"), Status::Pass);
    assert_eq!(status(&report, "codex (parent locally)"), Status::Pass);
    assert_eq!(status(&report, "Agent contract"), Status::Fail);
    assert_eq!(status(&report, "Repository list"), Status::Fail);
    assert_eq!(status(&report, "codex (worker on local)"), Status::Pass);
    assert_eq!(status(&report, "claude (worker on local)"), Status::Pass);
    #[cfg(target_os = "linux")]
    assert_eq!(status(&report, "Machine local sandbox"), Status::Fail);
    assert!(!report.text().contains("private-secret"));
    fixture.pristine();
}
