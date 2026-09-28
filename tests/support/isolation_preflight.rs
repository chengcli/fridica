use super::*;
use fridica::doctor::{self, Check};
use std::os::unix::fs::PermissionsExt;

fn context(f: &Fixture) -> LoadContext {
    LoadContext {
        home: f.home.clone(),
        runtime_dir: None,
        uid: users::get_current_uid(),
        protected: vec![],
    }
}
fn launch(f: &Fixture) -> process::Launch {
    Isolation::new(&f.config, &[])
        .unwrap()
        .preflight(&f.transport(), &f.workspace, BTreeMap::new())
        .unwrap()
}

#[tokio::test]
async fn local_probe_and_cli_do_not_provision_state_or_start_backends() {
    let f = Fixture::new();
    let source = std::fs::read(&f.config.path).unwrap();
    // Fresh backend homes are valid; the doctor must not create defaults.
    let report = doctor::isolation(
        &f.config,
        &context(&f),
        BTreeMap::new(),
        "local",
        "project",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(report.passed(), "{report:?}");
    assert!(!report.active_launch_ready);
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["doctor-isolation", "--config"])
        .arg(&f.config.path)
        .args(["--machine", "local", "--workspace", "project"])
        .env_clear()
        .env("HOME", &f.home)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["check"], "passed");
    assert_eq!(result["active_launch_ready"], false);
    assert_eq!(std::fs::read(&f.config.path).unwrap(), source);
    assert!(!f.config.state.path.exists());
    assert!(!f.config.state.control_socket.exists());
    assert_eq!(std::fs::read_dir(&f.home).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&f.workspace).unwrap().count(), 0);
}

#[tokio::test]
async fn probe_validates_settings_without_executing_or_rewriting_them() {
    let f = Fixture::new();
    std::fs::create_dir(f.home.join(".codex")).unwrap();
    let settings = f.home.join(".codex/config.toml");
    let source = "[mcp_servers.fridica]\ncommand='never-run-me'\nenv={SECRET='private-value'}\n";
    std::fs::write(&settings, source).unwrap();
    let claude_source =
        r#"{"mcpServers":{"fridica":{"command":"never-run-me","env":{"SECRET":"private-value"}}}}"#;
    std::fs::write(f.home.join(".claude.json"), claude_source).unwrap();
    assert_eq!(
        doctor::run_probe(launch(&f), Duration::from_secs(10)).await,
        Check::Passed
    );
    assert_eq!(std::fs::read_to_string(&settings).unwrap(), source);
    assert_eq!(
        std::fs::read_to_string(f.home.join(".claude.json")).unwrap(),
        claude_source
    );
    assert!(!f.home.join(".claude").exists());
    std::fs::write(&settings, "MALFORMED private-value").unwrap();
    assert_eq!(
        doctor::run_probe(launch(&f), Duration::from_secs(10)).await,
        Check::SettingsRefused
    );
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["doctor-isolation", "--config"])
        .arg(&f.config.path)
        .args(["--machine", "local", "--workspace", "project"])
        .env_clear()
        .env("HOME", &f.home)
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["check"], "settings_refused");
    for output in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(output).contains("private-value"));
        assert!(!String::from_utf8_lossy(output).contains(f.home.to_str().unwrap()));
    }
    std::fs::remove_file(&settings).unwrap();
    std::os::unix::fs::symlink(&f.config.path, &settings).unwrap();
    assert_eq!(
        doctor::run_probe(launch(&f), Duration::from_secs(10)).await,
        Check::SettingsRefused
    );
    assert!(!f.config.state.path.exists());
}

#[tokio::test]
async fn probe_refuses_missing_inventory_directories_and_never_creates_workspace() {
    let mut f = Fixture::new();
    std::fs::remove_dir(&f.workspace).unwrap();
    assert_eq!(
        doctor::run_probe(launch(&f), Duration::from_secs(10)).await,
        Check::InventoryRefused
    );
    assert!(!f.workspace.exists());
    std::fs::create_dir(&f.workspace).unwrap();
    // A separate missing directory (not nested in an already masked parent).
    let outside = tempfile::tempdir_in("/var/tmp").unwrap();
    f.config
        .isolation
        .private_files
        .push(outside.path().join("missing/key"));
    assert_eq!(
        doctor::run_probe(launch(&f), Duration::from_secs(10)).await,
        Check::InventoryRefused
    );
    assert!(!outside.path().join("missing").exists());
}

#[tokio::test]
async fn probe_reports_namespace_failure_without_raw_diagnostics() {
    let f = Fixture::new();
    let mut probe = launch(&f);
    // Simulate a host with no bwrap; this is a test-only helper replacement.
    probe.argv[5] = probe.argv[5].replace("/usr/bin/bwrap", "/no-such-tool-private-value");
    assert_eq!(
        doctor::run_probe(probe, Duration::from_secs(10)).await,
        Check::NamespaceFailed
    );
}

#[tokio::test]
async fn bounded_probe_requires_exact_success_and_exit_status() {
    for (code, expected) in [
        ("print('fridica-isolation:namespace'); print('fridica-isolation:ready'); raise SystemExit(1)", Check::RuntimeOrTransportFailed),
        ("print('private-value')", Check::RuntimeOrTransportFailed),
        ("import time; time.sleep(30)", Check::ProbeFailed),
        ("print('private-value'*10000)", Check::ProbeFailed),
    ] {
        let command = process::Launch {
            argv: vec!["/usr/bin/python3".into(), "-I".into(), "-S".into(), "-c".into(), code.into()],
            cwd: Some("/".into()), env: BTreeMap::new(),
        };
        let check = doctor::run_probe(command, Duration::from_millis(300)).await;
        assert_eq!(check, expected);
        assert!(!serde_json::to_string(&check).unwrap().contains("private-value"));
    }
}

#[tokio::test]
async fn remote_doctor_requires_inventory_then_probes_target_home_through_watchdog() {
    let mut f = Fixture::new();
    let remote_home = f.home.with_file_name("target-home");
    std::fs::create_dir_all(remote_home.join("private")).unwrap();
    std::fs::create_dir(remote_home.join("project")).unwrap();
    let machine = &mut f.config.machines.machines[0];
    machine.transport = "ssh".into();
    machine.host = "owner@synthetic".into();
    machine.workspaces[0].path = "~/project".into();
    let report = doctor::isolation(
        &f.config,
        &context(&f),
        BTreeMap::new(),
        "local",
        "project",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert_eq!(report.check, Check::MissingRemoteInventory);
    assert_eq!(std::fs::read_dir(&f.home).unwrap().count(), 0);

    f.config.isolation.remote.insert(
        "local".into(),
        fridica::config::isolation::Remote {
            host: "owner@synthetic".into(),
            private_files: vec!["~/private/state.db".into()],
        },
    );
    let tools = tempfile::tempdir().unwrap();
    let fake_ssh = tools.path().join("ssh");
    std::fs::write(&fake_ssh, format!(
        "#!/bin/sh\ncase \"$*\" in *ControlMaster=no*ControlPath=none*ControlPersist=no*StrictHostKeyChecking=yes*UpdateHostKeys=no*) ;; *) exit 255;; esac\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@synthetic || exit 255\nshift\nexport HOME={}\nexec /bin/sh -c \"$1\"\n",
        fridica::exec::shell::quote(remote_home.to_str().unwrap()),
    )).unwrap();
    std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
    let environment = BTreeMap::from([(
        "PATH".into(),
        format!("{}:/usr/bin:/bin", tools.path().display()).into(),
    )]);
    let report = doctor::isolation(
        &f.config,
        &context(&f),
        environment,
        "local",
        "project",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(report.passed(), "{report:?}");
    assert!(!remote_home.join(".codex").exists());
    assert!(!remote_home.join(".claude").exists());
    assert!(!remote_home.join("private/state.db").exists());
    assert_eq!(
        std::fs::read_dir(remote_home.join("project"))
            .unwrap()
            .count(),
        0
    );
}
