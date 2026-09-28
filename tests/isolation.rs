use fridica::exec::process;
#[cfg(target_os = "linux")]
use fridica::{
    config::{loader, Config, LoadContext},
    exec::{isolation::Isolation, local::LocalTransport},
};
use std::collections::BTreeMap;
#[cfg(target_os = "linux")]
use std::{path::PathBuf, time::Duration};

#[cfg(target_os = "linux")]
#[path = "support/remote_isolation.rs"]
mod remote;

#[cfg(target_os = "linux")]
struct Fixture {
    _dir: tempfile::TempDir,
    config: Config,
    home: PathBuf,
    workspace: PathBuf,
}
#[cfg(target_os = "linux")]
impl Fixture {
    fn new() -> Self {
        // /tmp is already hidden by the legacy layout. Use /var/tmp so this
        // exercises the new private-directory masks, not that existing tmpfs.
        let dir = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        let workspace = root.join("project 'quoted'");
        for p in [&home, &workspace, &root.join("private")] {
            std::fs::create_dir_all(p).unwrap();
        }
        let file = root.join("config.toml");
        let source = format!(
            r#"
[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local.workspaces]
project={}
[state]
path="private/state.db"
control_socket="private/control.sock"
"#,
            serde_json::json!(workspace)
        );
        std::fs::write(&file, &source).unwrap();
        let config = loader::parse(
            &source,
            &file,
            &LoadContext {
                home: home.clone(),
                runtime_dir: None,
                uid: users::get_current_uid(),
                protected: vec![],
            },
        )
        .unwrap();
        Self {
            _dir: dir,
            config,
            home,
            workspace,
        }
    }
    fn transport(&self) -> LocalTransport {
        LocalTransport {
            machine: self.config.machines.machines[0].clone(),
            home: self.home.clone(),
            excluded_env: vec![],
        }
    }
    fn command(&self, code: &str) -> process::Launch {
        Isolation::new(&self.config, &[])
            .unwrap()
            .launch(
                &self.transport(),
                vec![
                    "/usr/bin/python3".into(),
                    "-I".into(),
                    "-S".into(),
                    "-c".into(),
                    code.into(),
                ],
                &self.workspace,
                BTreeMap::new(),
                false,
            )
            .unwrap()
    }
}

#[test]
fn control_credentials_are_removed_after_overrides_without_changing_owner_backend_auth() {
    let env = process::scrubbed_environment(
        [
            ("FRIDICA_MCP_KEY".into(), "private-key".into()),
            ("fridica_control_token".into(), "private-token".into()),
            ("ANTHROPIC_API_KEY".into(), "owner-auth".into()),
        ],
        &[],
        &BTreeMap::from([
            ("FRIDICA_OVERSEER_CAPABILITY".into(), "override".into()),
            ("CUDA_VISIBLE_DEVICES".into(), "0".into()),
        ]),
    );
    assert_eq!(env.len(), 2);
    assert_eq!(
        env.get(std::ffi::OsStr::new("ANTHROPIC_API_KEY")).unwrap(),
        "owner-auth"
    );
    assert_eq!(
        env.get(std::ffi::OsStr::new("CUDA_VISIBLE_DEVICES"))
            .unwrap(),
        "0"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn confined_worker_cannot_read_state_or_connect_control_but_keeps_workspace_and_backend_state(
) {
    use std::os::unix::net::{UnixListener, UnixStream};
    let f = Fixture::new();
    std::fs::write(&f.config.state.path, "private database").unwrap();
    std::fs::write(f.config.state.path.with_extension("db-wal"), "private WAL").unwrap();
    std::fs::write(
        f.config.state.path.parent().unwrap().join("mcp.key"),
        "private capability",
    )
    .unwrap();
    let listener = UnixListener::bind(&f.config.state.control_socket).unwrap();
    let _probe = UnixStream::connect(&f.config.state.control_socket).unwrap();
    listener.accept().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::os::unix::fs::symlink(&f.config.state.path, f.workspace.join("db-alias")).unwrap();
    std::os::unix::fs::symlink(
        &f.config.state.control_socket,
        f.workspace.join("socket-alias"),
    )
    .unwrap();
    let checks = format!(
        r#"
import os, pathlib, socket
secret = pathlib.Path({db})
for p in [secret, pathlib.Path(str(secret)+'-wal'), secret.parent/'mcp.key', pathlib.Path('db-alias'), pathlib.Path({config}), pathlib.Path('/proc/1/root')/str(secret).lstrip('/')]:
    try:
        p.read_bytes()
    except (FileNotFoundError, PermissionError):
        pass
    else:
        raise AssertionError('private file readable')
for p in [{socket}, 'socket-alias']:
    s = socket.socket(socket.AF_UNIX)
    try:
        s.connect(p)
    except OSError:
        pass
    else:
        raise AssertionError('control socket reachable')
    finally:
        s.close()
pathlib.Path('result').write_text('workspace writable')
home = pathlib.Path(os.environ['HOME'])
(home/'.codex/session').write_text('backend state writable')
for p in [home/'.codex/config.toml', home/'.claude/settings.json', home/'.claude/hooks/new-hook']:
    try:
        p.write_text('unsafe')
    except OSError:
        pass
    else:
        raise AssertionError('backend settings writable')
for name in os.listdir('/proc/self/fd'):
    if int(name) > 2:
        try:
            os.fstat(int(name))
        except OSError:
            pass
        else:
            raise AssertionError('mount descriptor leaked into backend')
print('isolated')
"#,
        db = serde_json::json!(f.config.state.path),
        config = serde_json::json!(f.config.path),
        socket = serde_json::json!(f.config.state.control_socket)
    );
    let result = process::run_once(f.command(&checks), vec![], Duration::from_secs(10), 16384)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(result.stdout, b"isolated\n");
    assert_eq!(
        std::fs::read_to_string(f.workspace.join("result")).unwrap(),
        "workspace writable"
    );
    assert_eq!(
        std::fs::read_to_string(f.home.join(".codex/session")).unwrap(),
        "backend state writable"
    );
    assert_eq!(
        std::fs::read_to_string(&f.config.state.path).unwrap(),
        "private database"
    );
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn symlinked_workspaces_settings_and_private_remounts_refuse_before_backend_start() {
    for case in [
        "workspace",
        "settings",
        "private_workspace",
        "private_backend",
        "hardlinked_private",
        "double_slash",
    ] {
        let mut f = Fixture::new();
        let sentinel = f.workspace.join("started");
        let code = format!(
            "open({}, 'w').write('started')",
            serde_json::json!(sentinel)
        );
        match case {
            "workspace" => {
                std::fs::remove_dir(&f.workspace).unwrap();
                std::os::unix::fs::symlink(f.config.state.path.parent().unwrap(), &f.workspace)
                    .unwrap();
            }
            "settings" => {
                std::fs::create_dir_all(f.home.join(".codex")).unwrap();
                std::fs::write(&f.config.state.path, "secret").unwrap();
                std::os::unix::fs::symlink(&f.config.state.path, f.home.join(".codex/config.toml"))
                    .unwrap();
            }
            "private_workspace" => {
                f.config.state.path = f.workspace.join("state.db");
            }
            "private_backend" => {
                f.config.state.path = f.home.join(".codex/state.db");
            }
            "hardlinked_private" => {
                std::fs::write(&f.config.state.path, "private database").unwrap();
                std::fs::hard_link(&f.config.state.path, f.workspace.join("stolen")).unwrap();
            }
            "double_slash" => {
                f.workspace = PathBuf::from(format!(
                    "/{}",
                    f.config.state.path.parent().unwrap().display()
                ));
            }
            _ => unreachable!(),
        }
        let result = process::run_once(f.command(&code), vec![], Duration::from_secs(5), 4096)
            .await
            .unwrap();
        assert_eq!(
            result.returncode,
            97,
            "{case}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stderr, b"fridica worker isolation: setup refused\n");
        assert!(!sentinel.exists());
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn replacing_workspace_after_mount_source_open_cannot_expose_private_directory() {
    let f = Fixture::new();
    std::fs::write(&f.config.state.path, "private database").unwrap();
    let moved = f.workspace.with_file_name("pinned-original");
    let mut launch = f.command("from pathlib import Path; assert not Path('state.db').exists(); Path('result').write_text('pinned')");
    // Deterministically replace the pathname after the helper has opened every
    // mount source, immediately before the real bubblewrap exec. Production
    // exposes no hook or environment override for this interleaving.
    launch.argv[4] = format!(
        r#"
import os
original_exec = os.execve
def replace_before_exec(program, argv, env):
    os.rename({workspace}, {moved})
    os.symlink({private}, {workspace})
    original_exec(program, argv, env)
os.execve = replace_before_exec
{helper}
"#,
        workspace = serde_json::json!(f.workspace),
        moved = serde_json::json!(moved),
        private = serde_json::json!(f.config.state.path.parent().unwrap()),
        helper = fridica::exec::isolation::HELPER
    );
    let result = process::run_once(launch, vec![], Duration::from_secs(5), 4096)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(moved.join("result")).unwrap(),
        "pinned"
    );
    assert!(!f
        .config
        .state
        .path
        .parent()
        .unwrap()
        .join("result")
        .exists());
    assert_eq!(
        std::fs::read_to_string(&f.config.state.path).unwrap(),
        "private database"
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn system_launcher_applies_isolation_only_to_confined_local_workers() {
    use fridica::workers::{
        jsonl::{Launcher, SystemLauncher},
        protocol::WorkerSpec,
    };
    let f = Fixture::new();
    let launcher = SystemLauncher {
        home: f.home.clone(),
        environment: BTreeMap::from([("FRIDICA_MCP_KEY".into(), "must-not-leak".into())]),
        ssh_control_directory: f.home.join("ssh"),
        isolation: Isolation::new(&f.config, &[]).unwrap(),
    };
    let mut spec: WorkerSpec = serde_json::from_value(serde_json::json!({"worker_id":"w", "machine":f.config.machines.machines[0], "workspace":f.config.machines.machines[0].workspaces[0], "backend":"codex", "instructions":"", "model":"", "reasoning_effort":"", "job_timeout":30, "idle_timeout":30, "excluded_env":[], "slot":0})).unwrap();
    spec.workspace.policy.gpu_confine = Some(true);
    let launch = launcher.launch(&spec, vec!["/bin/true".into()]).unwrap();
    assert_eq!(&launch.argv[..3], &["/usr/bin/python3", "-I", "-S"]);
    assert!(!launch
        .env
        .contains_key(std::ffi::OsStr::new("FRIDICA_MCP_KEY")));
    let result = process::run_once(launch, vec![], Duration::from_secs(5), 4096)
        .await
        .unwrap();
    assert_eq!(
        result.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    spec.workspace.policy.gpu_confine = Some(false);
    let launch = launcher.launch(&spec, vec!["/bin/true".into()]).unwrap();
    assert_eq!(launch.argv, vec!["/bin/true"]);
    assert!(!launch
        .env
        .contains_key(std::ffi::OsStr::new("FRIDICA_MCP_KEY")));
}
