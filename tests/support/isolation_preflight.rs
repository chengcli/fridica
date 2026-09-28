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
            settings_files: vec![],
            mcp_inventory_complete: false,
            host: "owner@synthetic".into(),
            private_files: vec!["~/private/state.db".into()],
        },
    );
    let tools = tempfile::tempdir().unwrap();
    std::fs::set_permissions(tools.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
        environment.clone(),
        "local",
        "project",
        Duration::from_secs(10),
    )
    .await
    .unwrap();
    assert!(report.passed(), "{report:?}");
    use fridica::workers::jsonl::{Launcher, SystemLauncher};
    let launcher = SystemLauncher::from_config(
        &f.config,
        f.home.clone(),
        environment,
        tools.path().to_owned(),
    )
    .unwrap();
    let spec = worker_spec(&f);
    launcher
        .admit(std::sync::Arc::new(f.config.clone()), spec.clone())
        .await
        .unwrap();
    let mut changed = f.config.clone();
    changed.machines.machines[0].host = "owner@replacement".into();
    changed.isolation.remote.get_mut("local").unwrap().host = "owner@replacement".into();
    assert!(launcher.validate_config(&changed).is_err());
    std::fs::remove_dir(remote_home.join("private")).unwrap();
    assert_eq!(
        launcher
            .admit(std::sync::Arc::new(f.config.clone()), spec)
            .await
            .unwrap_err()
            .code,
        "worker_isolation_inventory_refused"
    );
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

fn worker_spec(f: &Fixture) -> fridica::workers::protocol::WorkerSpec {
    let machine = &f.config.machines.machines[0];
    let mut workspace = machine.workspaces[0].for_slot(1);
    workspace.policy.gpu_confine = Some(true);
    serde_json::from_value(serde_json::json!({"worker_id":"w", "machine":machine,
        "workspace":workspace, "backend":"codex", "instructions":"", "model":"",
        "reasoning_effort":"", "job_timeout":30, "idle_timeout":30, "excluded_env":[], "slot":1}))
    .unwrap()
}

#[tokio::test]
async fn automatic_admission_probes_without_provisioning_and_rechecks_changed_settings() {
    use fridica::workers::jsonl::{Launcher, SystemLauncher};
    use std::sync::Arc;
    let f = Fixture::new();
    let launcher = SystemLauncher::from_config(
        &f.config,
        f.home.clone(),
        BTreeMap::new(),
        f.home.join("ssh"),
    )
    .unwrap();
    let spec = worker_spec(&f);
    launcher
        .admit(Arc::new(f.config.clone()), spec.clone())
        .await
        .unwrap();
    assert!(!spec.workspace.path.exists());
    assert!(!f.home.join(".codex").exists());
    std::fs::create_dir(f.home.join(".codex")).unwrap();
    std::fs::write(f.home.join(".codex/config.toml"), "invalid private-value").unwrap();
    let error = launcher
        .admit(Arc::new(f.config.clone()), spec.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "worker_isolation_settings_refused");
    assert_eq!(error.kind, fridica::core::worker::Failure::Refusal);
    assert!(!spec.workspace.path.exists());
    assert!(!f.config.state.path.exists());
    // Ordinary workers retain their policy and do not invoke a namespace probe.
    let mut ordinary = spec;
    ordinary.workspace.policy.gpu_confine = Some(false);
    launcher
        .admit(Arc::new(f.config.clone()), ordinary)
        .await
        .unwrap();
}

#[tokio::test]
async fn immutable_launcher_rejects_inventory_identity_and_private_path_changes() {
    use fridica::workers::jsonl::{Launcher, SystemLauncher};
    use std::sync::Arc;
    let f = Fixture::new();
    let launcher = SystemLauncher::from_config(
        &f.config,
        f.home.clone(),
        BTreeMap::new(),
        f.home.join("ssh"),
    )
    .unwrap();
    for index in 0..4 {
        let mut config = f.config.clone();
        match index {
            0 => config
                .isolation
                .private_files
                .push(f.home.join("private/key")),
            1 => config.isolation.mcp_aliases.push("owner-wrapper".into()),
            2 => config
                .isolation
                .mcp_urls
                .push("http://localhost:8765/mcp".into()),
            _ => config.state.control_socket = f.home.join("private/new-control.sock"),
        }
        assert!(launcher.validate_config(&config).is_err());
        let error = launcher
            .admit(Arc::new(config), worker_spec(&f))
            .await
            .unwrap_err();
        assert_eq!(error.code, "worker_isolation_configuration_changed");
    }
    assert_eq!(std::fs::read_dir(&f.home).unwrap().count(), 0);
}

#[tokio::test]
async fn cancelled_probe_retains_capacity_until_stubborn_child_is_reaped() {
    use fridica::exec::isolation::run_probe_with_permit;
    use std::sync::Arc;
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("pid");
    let command = process::Launch {
        argv: vec!["/usr/bin/python3".into(), "-I".into(), "-S".into(), "-c".into(),
            "import os,pathlib,signal,sys,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); pathlib.Path(sys.argv[1]).write_text(str(os.getpid())); time.sleep(30)".into(),
            pid_file.to_str().unwrap().into()],
        cwd: Some("/".into()), env: BTreeMap::new(),
    };
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = permits.clone().acquire_owned().await.unwrap();
    let task = tokio::spawn(run_probe_with_permit(
        command,
        Duration::from_secs(20),
        permit,
    ));
    tokio::time::timeout(Duration::from_secs(5), async {
        while std::fs::read_to_string(&pid_file)
            .unwrap_or_default()
            .is_empty()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let pid = std::fs::read_to_string(pid_file).unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(
        tokio::time::timeout(Duration::from_millis(100), permits.acquire())
            .await
            .is_err()
    );
    let _next = tokio::time::timeout(Duration::from_secs(5), permits.acquire())
        .await
        .unwrap()
        .unwrap();
    assert!(!std::path::Path::new("/proc").join(pid.trim()).exists());
}

#[tokio::test]
async fn explicit_extra_source_is_sanitized_in_confined_mounts_and_missing_source_refuses() {
    let mut f = Fixture::new();
    let extra = f.workspace.join("extra.json");
    let source = r#"{"mcpServers":{"outside-default-layers":{"command":"fridica","env":{"KEY":"private-source"}}}}"#;
    std::fs::write(&extra, source).unwrap();
    f.config.isolation.settings_files.push(extra.clone());
    let launch = f.command("import json, pathlib; assert json.loads(pathlib.Path('extra.json').read_text()) == {'mcpServers': {}}");
    let result = process::run_once(launch, vec![], Duration::from_secs(10), 4096)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(std::fs::read_to_string(&extra).unwrap(), source);
    std::fs::remove_file(&extra).unwrap();
    assert_eq!(
        doctor::run_probe(super::preflight::launch(&f), Duration::from_secs(10)).await,
        Check::SettingsRefused
    );
}
