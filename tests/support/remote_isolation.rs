use super::*;
use fridica::{
    config::registry::Machine,
    exec::{shell, ssh},
    workers::{
        jsonl::{Launcher, SystemLauncher},
        protocol::WorkerSpec,
    },
};
use std::os::unix::fs::PermissionsExt;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

struct Remote {
    local: Fixture,
    home: PathBuf,
    workspace: PathBuf,
    fake_ssh: PathBuf,
    spec: WorkerSpec,
    launcher: SystemLauncher,
}
impl Remote {
    fn new() -> Self {
        let local = Fixture::new();
        let home = local.home.with_file_name("remote home");
        std::fs::create_dir_all(home.join("private")).unwrap();
        let relative = "project 'quoted' $(touch INJECTED)/worker-1";
        let workspace = home.join(relative);
        let mut machine: Machine = local.config.machines.machines[0].clone();
        machine.name = "remote".into();
        machine.transport = "ssh".into();
        machine.host = "owner@target".into();
        machine.resources.cpus = Some(2);
        let mut spec: WorkerSpec = serde_json::from_value(serde_json::json!({"worker_id":"remote-worker", "machine":machine, "workspace":machine.workspaces[0], "backend":"codex", "instructions":"", "model":"", "reasoning_effort":"", "job_timeout":30, "idle_timeout":30, "excluded_env":["OWNER_CONTROL_SECRET"], "slot":1})).unwrap();
        spec.workspace.path = format!("~/{relative}").into();
        spec.workspace.subfolders = true;
        spec.workspace.policy.gpu_confine = Some(true);
        let fake_ssh = local.home.join("fake-ssh");
        std::fs::write(
            &fake_ssh,
            format!(
                "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@target || exit 255\nshift\nexport HOME={}\nexport FRIDICA_MCP_KEY=remote-capability fridica_control_token=remote-token OWNER_CONTROL_SECRET=private SLACK_TOKEN=slack-private UNNAMED_TOKEN=xapp-private ANTHROPIC_API_KEY=backend-auth\nexec /bin/sh -c \"$1\"\n",
                shell::quote(home.to_str().unwrap())
            ),
        ).unwrap();
        std::fs::set_permissions(&fake_ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let control = ssh::control_directory(Some(&local.home), users::get_current_uid()).unwrap();
        let mut provisioned = local.config.clone();
        provisioned.machines.machines = vec![machine.clone()];
        provisioned.machines.default = machine.name.clone();
        provisioned.isolation.remote.insert(
            machine.name.clone(),
            fridica::config::isolation::Remote {
                settings_files: vec![],
                mcp_inventory_complete: true,
                host: machine.host.clone(),
            },
        );
        let launcher = SystemLauncher::from_config(
            &provisioned,
            local.home.clone(),
            BTreeMap::from([
                ("HOME".into(), local.home.clone().into_os_string()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ]),
            control,
        )
        .unwrap();
        Self {
            local,
            home,
            workspace,
            fake_ssh,
            spec,
            launcher,
        }
    }
    fn launch(&self, code: &str) -> process::Launch {
        let mut launch = self
            .launcher
            .launch(
                &self.spec,
                vec![
                    "/usr/bin/python3".into(),
                    "-I".into(),
                    "-S".into(),
                    "-c".into(),
                    code.into(),
                ],
            )
            .unwrap();
        assert_eq!(launch.argv[0], "ssh");
        assert!(launch.argv.iter().any(|a| a == "BatchMode=yes"));
        assert!(!launch
            .argv
            .iter()
            .any(|a| a == "-A" || a.starts_with("ForwardAgent=")));
        launch.argv[0] = self.fake_ssh.to_str().unwrap().into();
        launch
    }
}

// Keep the protocol input channel open until the backend exits, as a real
// app-server owner does. Closing it early intentionally invokes the watchdog.
async fn complete(launch: process::Launch) -> (i32, String, String) {
    let mut child = process::Process::start(&launch).unwrap();
    let mut stdin = child.stdin().unwrap();
    stdin.write_all(b"request\n").await.unwrap();
    let mut stdout = child.stdout().unwrap();
    let mut stderr = child.stderr().unwrap();
    let mut out = String::new();
    let mut err = String::new();
    let (status, read_out, read_err) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(
            child.wait(),
            stdout.read_to_string(&mut out),
            stderr.read_to_string(&mut err)
        )
    })
    .await
    .unwrap();
    read_out.unwrap();
    read_err.unwrap();
    drop(stdin);
    (status.unwrap().code().unwrap_or(-1), out, err)
}

#[tokio::test]
async fn remote_worker_keeps_owner_files_but_scrubs_environment_and_mcp_settings() {
    let r = Remote::new();
    std::fs::create_dir_all(r.home.join(".codex")).unwrap();
    let remote_settings =
        "[mcp_servers.target_alias]\ncommand='fridica'\nenv={TOKEN='remote-mcp-secret'}\n";
    std::fs::write(r.home.join(".codex/config.toml"), remote_settings).unwrap();
    // The owner's files on the target (git/gh/ssh credentials, data) stay visible.
    std::fs::write(r.home.join("private/state.db"), "remote database").unwrap();
    std::fs::write(r.home.join("private/state.db-wal"), "remote WAL").unwrap();
    let code = format!(
        r#"
import os, pathlib, sys
assert sys.stdin.readline() == 'request\n'
home = pathlib.Path(os.environ['HOME'])
assert home == pathlib.Path({home})
assert pathlib.Path.cwd() == pathlib.Path({workspace})
assert os.environ['OMP_NUM_THREADS'] == '2'
assert os.environ['ANTHROPIC_API_KEY'] == 'backend-auth'
settings = (home/'.codex/config.toml').read_text()
assert 'remote-mcp-secret' not in settings and '"enabled" = false' in settings
for key in ['FRIDICA_MCP_KEY', 'fridica_control_token', 'OWNER_CONTROL_SECRET', 'SLACK_TOKEN', 'UNNAMED_TOKEN']:
    assert key not in os.environ
assert (home/'private/state.db').read_text() == 'remote database'
assert (home/'private/state.db-wal').read_text() == 'remote WAL'
pathlib.Path('result').write_text('remote workspace writable')
(home/'.codex/session').write_text('remote session writable')
try:
    (home/'.codex/config.toml').write_text('unsafe')
except OSError:
    pass
else:
    raise AssertionError('remote settings writable')
for name in os.listdir('/proc/self/fd'):
    if int(name) > 2:
        try:
            os.fstat(int(name))
        except OSError:
            pass
        else:
            raise AssertionError('remote mount/channel descriptor leaked')
print('remote isolated')
sys.exit(42)
"#,
        home = serde_json::json!(r.home),
        workspace = serde_json::json!(r.workspace),
    );
    let result = complete(r.launch(&code)).await;
    assert_eq!(result, (42, "remote isolated\n".into(), String::new()));
    assert_eq!(
        std::fs::read_to_string(r.workspace.join("result")).unwrap(),
        "remote workspace writable"
    );
    assert!(r.home.join(".codex/session").exists());
    assert_eq!(
        std::fs::read_to_string(r.home.join(".codex/config.toml")).unwrap(),
        remote_settings
    );
    assert!(!r.home.join("INJECTED").exists());
    assert!(!r.local.home.join(".codex").exists());
}

#[test]
fn mismatched_remote_bindings_refuse_and_confinement_needs_no_private_inventory() {
    let mut r = Remote::new();
    let command = vec!["/bin/true".into()];
    r.spec.machine.host = "different-target".into();
    assert!(r.launcher.launch(&r.spec, command.clone()).is_err());
    r.spec.machine.host = "owner@target".into();
    // No [isolation.remote] table at all: confined SSH still launches.
    r.launcher.isolation = Isolation::new(&r.local.config, &[]).unwrap();
    assert!(r.launcher.launch(&r.spec, command.clone()).is_ok());
    r.spec.workspace.policy.gpu_confine = Some(false);
    assert!(r.launcher.launch(&r.spec, command).is_ok());
}

#[tokio::test]
async fn remote_symlinked_workspace_refuses_before_backend_start() {
    let r = Remote::new();
    std::fs::create_dir_all(r.workspace.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(r.home.join("private"), &r.workspace).unwrap();
    let result = complete(r.launch("print('backend started')")).await;
    assert_eq!(
        result,
        (
            97,
            String::new(),
            "fridica worker isolation: setup refused\n".into()
        )
    );
}

#[tokio::test]
async fn remote_watchdog_stops_confined_descendants_on_channel_eof_and_wrapper_signal() {
    for stop in ["eof", "signal"] {
        let r = Remote::new();
        let code = r#"
import pathlib, signal, subprocess, sys, time
signal.signal(signal.SIGTERM, signal.SIG_IGN)
tool = subprocess.Popen([sys.executable, '-I', '-S', '-c', "import pathlib, signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN)\nwhile True:\n pathlib.Path('heartbeat').write_text(str(time.monotonic_ns()))\n time.sleep(0.02)"])
while not pathlib.Path('heartbeat').exists():
    time.sleep(0.01)
print('ready', flush=True)
while True:
    time.sleep(1)
"#;
        let mut child = process::Process::start(&r.launch(code)).unwrap();
        let stdin = child.stdin().unwrap();
        let mut stdout = BufReader::new(child.stdout().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "ready\n");
        if stop == "eof" {
            drop(stdin);
        } else {
            rustix::process::kill_process(
                rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
                rustix::process::Signal::TERM,
            )
            .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(8), child.wait())
            .await
            .unwrap()
            .unwrap();
        let before = std::fs::read(r.workspace.join("heartbeat")).unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            std::fs::read(r.workspace.join("heartbeat")).unwrap(),
            before,
            "{stop}: descendant survived"
        );
    }
}
