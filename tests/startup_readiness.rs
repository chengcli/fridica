use fridica::{
    config::{self, loader, Config, LoadContext},
    daemon,
    doctor::{readiness, Check},
    exec::{process, shell},
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::PathBuf,
    time::Duration,
};
use tokio::sync::watch;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    context: LoadContext,
    config: Config,
    env: BTreeMap<OsString, OsString>,
}
impl Fixture {
    fn new() -> Self {
        #[cfg(target_os = "linux")]
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        #[cfg(not(target_os = "linux"))]
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["home", "bin", "private", "project"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        std::fs::set_permissions(root.join("private"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        let context = LoadContext {
            home: root.join("home"),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        };
        let file = root.join("private/config.toml");
        let source = format!(
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
[isolation]
mcp_inventory_complete=true
"#,
            json!(root.join("project"))
        );
        std::fs::write(&file, &source).unwrap();
        let config = loader::parse(&source, &file, &context).unwrap();
        // Any backend invocation (including --version) fails this fixture.
        for backend in ["codex", "claude"] {
            let path = root.join("bin").join(backend);
            std::fs::write(
                &path,
                format!(
                    "#!/bin/sh\ntouch {}\nexit 91\n",
                    shell::quote(root.join("backend-started").to_str().unwrap())
                ),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let env = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("HOME".into(), context.home.clone().into_os_string()),
        ]);
        Self {
            _dir: dir,
            root,
            context,
            config,
            env,
        }
    }
    async fn check(&self) -> readiness::Report {
        let (_sender, stop) = watch::channel(false);
        readiness::check(
            &self.config,
            &self.context,
            self.env.clone(),
            Duration::from_secs(5),
            stop,
        )
        .await
        .unwrap()
    }
    fn pristine(&self) {
        assert!(!self.config.state.path.exists());
        assert!(!self.config.state.control_socket.exists());
        assert!(!self.root.join("backend-started").exists());
        assert!(!self.context.home.join(".codex").exists());
        assert!(!self.root.join("project/worker1").exists());
    }
    async fn cli(&self, extra: &[&str]) -> process::Completed {
        let mut argv = vec![
            env!("CARGO_BIN_EXE_fridica").into(),
            "start".into(),
            "--check-ready".into(),
            "--config".into(),
            self.config.path.to_str().unwrap().into(),
            "--timeout".into(),
            "2".into(),
        ];
        argv.extend(extra.iter().map(|s| s.to_string()));
        process::run_once(
            process::Launch {
                argv,
                env: self.env.clone(),
                cwd: None,
            },
            vec![],
            Duration::from_secs(15),
            16384,
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn cli_checks_all_backends_without_tokens_state_or_backend_invocation() {
    let f = Fixture::new();
    let output = f.cli(&[]).await;
    assert!(
        output.returncode == 0,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["startup_checks_passed"], true);
    assert_eq!(report["active_launch_ready"], false);
    assert_eq!(report["targets"].as_array().unwrap().len(), 2);
    assert_eq!(report["config_fingerprint"], f.config.fingerprint);
    assert_eq!(report["remaining_gates"].as_array().unwrap().len(), 1);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(f.root.to_str().unwrap()));
    f.pristine();
    let output = f.cli(&["--observe-only"]).await;
    assert_ne!(output.returncode, 0);
    let output = process::run_once(
        process::Launch {
            argv: vec![
                env!("CARGO_BIN_EXE_fridica").into(),
                "start".into(),
                "--config".into(),
                f.config.path.to_str().unwrap().into(),
            ],
            cwd: None,
            env: f.env.clone(),
        },
        vec![],
        Duration::from_secs(5),
        16384,
    )
    .await
    .unwrap();
    assert_ne!(output.returncode, 0); // Passing preparation does not unlock active CLI.
    f.pristine();
}

#[tokio::test]
async fn missing_executables_workspaces_and_unreviewed_sources_are_fixed_blockers() {
    let mut f = Fixture::new();
    f.config.isolation.mcp_inventory_complete = false;
    let report = f.check().await;
    assert!(!report.startup_checks_passed);
    assert!(report
        .targets
        .iter()
        .all(|t| t.check == Check::McpInventoryUnreviewed));
    f.config.isolation.mcp_inventory_complete = true;
    std::fs::remove_file(f.root.join("bin/codex")).unwrap();
    assert_eq!(f.check().await.targets[0].check, Check::BackendMissing);
    std::fs::remove_dir(f.root.join("project")).unwrap();
    let report = f.check().await;
    assert_eq!(report.targets[1].check, Check::WorkspaceRefused);
    std::fs::remove_file(f.root.join("bin/claude")).unwrap();
    assert_eq!(f.check().await.parent, Check::BackendMissing);
    f.pristine();
}

#[tokio::test]
async fn extra_source_files_are_required_rechecked_and_never_exposed_in_reports() {
    let mut f = Fixture::new();
    let source = f.root.join("extra-owner-settings.json");
    f.config.isolation.settings_files.push(source.clone());
    assert_eq!(f.check().await.targets[0].check, Check::SettingsRefused);
    std::fs::write(
        &source,
        r#"{"mcpServers":{"opaque":{"command":"fridica","env":{"KEY":"private-source"}}}}"#,
    )
    .unwrap();
    assert!(f.check().await.startup_checks_passed);
    std::fs::write(&source, "MALFORMED private-source").unwrap();
    let report = f.check().await;
    assert_eq!(report.targets[0].check, Check::SettingsRefused);
    let text = serde_json::to_string(&report).unwrap();
    assert!(!text.contains("private-source") && !text.contains("extra-owner-settings"));
    f.pristine();
}

#[tokio::test]
async fn active_host_refuses_before_state_or_network_and_pre_cancelled_start_has_no_effects() {
    let mut f = Fixture::new();
    f.config.isolation.mcp_inventory_complete = false;
    let credentials = || {
        daemon::Credentials::read(&f.config, |name| {
            Some(
                if name == f.config.slack.app_token_env {
                    "xapp-fixture"
                } else {
                    "xoxp-fixture"
                }
                .into(),
            )
        })
        .unwrap()
    };
    let (_sender, stop) = watch::channel(false);
    assert!(daemon::active(
        f.config.clone(),
        f.context.clone(),
        f.env.clone(),
        credentials(),
        stop
    )
    .await
    .is_err());
    f.pristine();
    let (_sender, stop) = watch::channel(true);
    daemon::active(
        f.config.clone(),
        f.context.clone(),
        f.env.clone(),
        credentials(),
        stop,
    )
    .await
    .unwrap();
    f.pristine();
}

#[tokio::test]
async fn remote_readiness_uses_target_sources_without_forwarding_local_inventory_or_running_backends(
) {
    let mut f = Fixture::new();
    let remote = f.root.join("target-home");
    std::fs::create_dir_all(remote.join("project")).unwrap();
    std::fs::write(
        remote.join("extra.json"),
        r#"{"mcpServers":{"remote":{"command":"fridica"}}}"#,
    )
    .unwrap();
    f.config.machines.machines[0].transport = "ssh".into();
    f.config.machines.machines[0].host = "owner@fixture".into();
    f.config.machines.machines[0].workspaces[0].path = "~/project".into();
    f.config
        .isolation
        .settings_files
        .push(f.root.join("missing-local-file.toml"));
    f.config.isolation.remote.insert(
        "local".into(),
        config::isolation::Remote {
            host: "owner@fixture".into(),
            settings_files: vec!["~/extra.json".into()],
            mcp_inventory_complete: true,
        },
    );
    let ssh = f.root.join("bin/ssh");
    std::fs::write(&ssh,format!("#!/bin/sh\ncase \"$*\" in *ControlMaster=no*ControlPath=none*StrictHostKeyChecking=yes*UpdateHostKeys=no*) ;; *) exit 255;; esac\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@fixture || exit 255\nshift\nexport HOME={}\nexec /bin/sh -c \"$1\"\n",shell::quote(remote.to_str().unwrap()))).unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(f.check().await.startup_checks_passed);
    std::fs::remove_file(remote.join("extra.json")).unwrap();
    assert_eq!(f.check().await.targets[0].check, Check::SettingsRefused);
    f.config
        .isolation
        .remote
        .get_mut("local")
        .unwrap()
        .mcp_inventory_complete = false;
    std::fs::remove_file(&ssh).unwrap();
    assert_eq!(
        f.check().await.targets[0].check,
        Check::McpInventoryUnreviewed
    );
    assert!(!remote.join(".codex").exists());
    f.pristine();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_readiness_checks_binary_visibility_inside_the_namespace_without_running_it() {
    let mut f = Fixture::new();
    f.config.machines.machines[0].workspaces[0]
        .policy
        .gpu_confine = Some(true);
    let report = f.check().await;
    assert!(report.startup_checks_passed, "{report:?}");
    assert!(report
        .targets
        .iter()
        .all(|t| t.isolation == Some(Check::Passed)));
    // Present on the host, but masked inside the worker's private-file boundary.
    std::fs::rename(f.root.join("bin/codex"), f.root.join("private/codex")).unwrap();
    f.env.insert(
        "PATH".into(),
        format!(
            "{}:{}:/usr/bin:/bin",
            f.root.join("private").display(),
            f.root.join("bin").display()
        )
        .into(),
    );
    let report = f.check().await;
    assert!(!report.startup_checks_passed);
    assert_eq!(report.targets[0].check, Check::BackendMissing);
    f.pristine();
}

#[tokio::test]
async fn stopping_readiness_reaps_the_current_probe_and_skips_remaining_backends() {
    let mut f = Fixture::new();
    f.config.machines.machines[0].transport = "ssh".into();
    f.config.machines.machines[0].host = "owner@fixture".into();
    f.config.isolation.remote.insert(
        "local".into(),
        config::isolation::Remote {
            host: "owner@fixture".into(),
            settings_files: vec![],
            mcp_inventory_complete: true,
        },
    );
    let marker = f.root.join("ssh-pids");
    let ssh = f.root.join("bin/ssh");
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\ntrap '' TERM\necho $$ >> {}\nexec /bin/sleep 60\n",
            shell::quote(marker.to_str().unwrap())
        ),
    )
    .unwrap();
    std::fs::set_permissions(ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (sender, stop) = watch::channel(false);
    let checking = readiness::check(
        &f.config,
        &f.context,
        f.env.clone(),
        Duration::from_secs(2),
        stop,
    );
    let stopping = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            while std::fs::read_to_string(&marker)
                .unwrap_or_default()
                .is_empty()
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        sender.send_replace(true);
    };
    let (report, ()) = tokio::time::timeout(Duration::from_secs(8), async {
        tokio::join!(checking, stopping)
    })
    .await
    .unwrap();
    let report = report.unwrap();
    assert!(report.cancelled && !report.startup_checks_passed);
    assert_eq!(report.targets.len(), 1);
    let pids = std::fs::read_to_string(marker).unwrap();
    assert_eq!(pids.lines().count(), 1);
    let pid = rustix::process::Pid::from_raw(pids.trim().parse().unwrap()).unwrap();
    assert_eq!(
        rustix::process::test_kill_process(pid),
        Err(rustix::io::Errno::SRCH)
    );
    f.pristine();
}

#[tokio::test]
async fn host_directory_permissions_and_owner_inputs_block_before_target_io() {
    let mut f = Fixture::new();
    std::fs::set_permissions(
        f.root.join("private"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let report = f.check().await;
    assert_eq!(report.host, Check::HostPathsRefused);
    assert!(report.targets.is_empty());
    std::fs::set_permissions(
        f.root.join("private"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    f.config.owner.contract = Some(f.root.join("missing-private-contract.md"));
    let report = f.check().await;
    assert_eq!(report.owner_inputs, Check::OwnerInputsRefused);
    assert!(report.targets.is_empty());
    assert!(!serde_json::to_string(&report)
        .unwrap()
        .contains("missing-private"));
    f.pristine();
}

// This child entry point exercises production active host wiring under an
// isolated environment. The parent holds a loopback proxy: no Slack connection
// can leave the fixture. Backend probes are scripted; model sessions must never start.
#[tokio::test]
async fn active_host_fixture_child() {
    let Some(path) = std::env::var_os("FRIDICA_TEST_ACTIVE_CONFIG") else {
        return;
    };
    let context = LoadContext::current().unwrap();
    let config = config::load(&PathBuf::from(path), &context).unwrap();
    let credentials = daemon::Credentials::read(&config, |name| std::env::var(name).ok()).unwrap();
    let mut term =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    let (sender, stop) = watch::channel(false);
    let running = daemon::active(
        config,
        context,
        std::env::vars_os().collect(),
        credentials,
        stop,
    );
    tokio::pin!(running);
    tokio::select! {
        result=&mut running=>result.unwrap(),
        _=term.recv()=>{sender.send_replace(true);running.await.unwrap();},
    }
}

#[tokio::test]
async fn prepared_active_host_checks_backend_auth_then_drains_without_model_sessions() {
    use fridica::control::client::Client;
    use rustix::process::{kill_process, Pid, Signal};
    use tokio::{io::AsyncReadExt, net::TcpListener};
    let f = Fixture::new();
    let backend = r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
args = sys.argv[1:]
if args == ['--help']:
    print('--input-format --permission-prompts --json-schema --setting-sources --strict-mcp-config --append-system-prompt --session-id dontAsk "auto"')
elif args == ['auth','status']:
    print('{"loggedIn":true}')
elif args == ['exec','--help']:
    print('--ignore-user-config --ignore-rules --output-schema --ephemeral')
elif args == ['login','status']:
    pass
elif args[:2] == ['app-server','generate-json-schema']:
    (Path(args[3]) / 'schema.json').write_text('turn/interrupt item/commandExecution/requestApproval outputSchema auto_review')
else:
    (Path(__file__).parent.parent / 'backend-started').touch()
    raise SystemExit(91)
"#;
    for name in ["claude", "codex"] {
        std::fs::write(f.root.join("bin").join(name), backend).unwrap();
    }
    for name in ["bwrap", "socat"] {
        let path = f.root.join("bin").join(name);
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut environment = f.env.clone();
    environment.extend([
        (
            "FRIDICA_TEST_ACTIVE_CONFIG".into(),
            f.config.path.clone().into_os_string(),
        ),
        (
            "XDG_RUNTIME_DIR".into(),
            f.root.join("private").into_os_string(),
        ),
        ("SLACK_APP_TOKEN".into(), "xapp-fixture".into()),
        ("SLACK_USER_TOKEN".into(), "xoxp-fixture".into()),
        (
            "HTTPS_PROXY".into(),
            format!("http://{}", proxy.local_addr().unwrap()).into(),
        ),
        ("NO_PROXY".into(), "".into()),
    ]);
    let launch = process::Launch {
        argv: vec![
            std::env::current_exe().unwrap().to_str().unwrap().into(),
            "--exact".into(),
            "active_host_fixture_child".into(),
            "--nocapture".into(),
        ],
        cwd: None,
        env: environment,
    };
    let mut child = process::Process::start(&launch).unwrap();
    let (mut connection, _) = tokio::time::timeout(Duration::from_secs(10), proxy.accept())
        .await
        .unwrap()
        .unwrap();
    let mut request = [0; 1024];
    let count = tokio::time::timeout(Duration::from_secs(3), connection.read(&mut request))
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(&request[..count]).starts_with("CONNECT slack.com:443 "));
    let client = Client::new(&f.config.state.control_socket, None).unwrap();
    assert_eq!(
        client.request("GET", "/status", None).await.unwrap()["observe_only"],
        false
    );
    assert!(!f.root.join("backend-started").exists());
    kill_process(Pid::from_raw(child.id() as i32).unwrap(), Signal::TERM).unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert!(!f.config.state.control_socket.exists());
    let db = rusqlite::Connection::open(&f.config.state.path).unwrap();
    assert!(db
        .query_row(
            "SELECT observe_only=0 AND slack_status='stopped' AND control_socket='' FROM runtime",
            [],
            |r| r.get::<_, bool>(0)
        )
        .unwrap());
    assert_eq!(
        db.query_row("SELECT count(*) FROM jobs", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}
