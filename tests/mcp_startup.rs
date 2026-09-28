//! Real target-side startup helpers with synthetic backends; no model or MCP I/O.
use fridica::{
    config::{loader, Config, LoadContext},
    exec::{process, shell},
    workers::{
        codex,
        jsonl::{Launcher, SystemLauncher},
        protocol::WorkerSpec,
    },
};
use serde_json::{json, Value};
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};

const BACKEND: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
args = sys.argv[1:]
for alias in json.loads(os.environ['DANGEROUS_ALIASES']):
    flag = 'mcp_servers.' + json.dumps(alias, ensure_ascii=False) + '.enabled=false'
    if flag not in args:
        pathlib.Path('mcp-started').write_text(alias)
        raise SystemExit(9)
assert 'mcp_servers."docs".enabled=false' not in args
assert not any(k.upper().startswith('FRIDICA_') or 'SLACK' in k.upper() for k in os.environ)
assert 'OWNER_SECRET' not in os.environ
assert os.environ['MODEL_AUTH'] == 'owner-model-auth'
assert os.environ['OMP_NUM_THREADS'] == '2'
# Ordinary filesystem access and persistent backend state remain available.
pathlib.Path(os.environ['HOME'], 'session-state').write_text('kept')
pathlib.Path('report.txt').write_text('done')
print(json.dumps({'args':args, 'cwd':os.getcwd(), 'home':os.environ['HOME'],
                  'codex_home':os.environ.get('CODEX_HOME'), 'unrestricted':True}))
"#;
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    config: Config,
    spec: WorkerSpec,
    env: BTreeMap<std::ffi::OsString, std::ffi::OsString>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let home = root.join("home");
        for name in ["home", "bin", "project", "private"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let source = "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[machines.local.resources]\ncpus=2\n[machines.local.policy]\nmode='full'\ngpu_confine=false\n[machines.local.workspaces]\nproject='project'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'\n";
        let config = loader::parse(
            source,
            &root.join("config.toml"),
            &LoadContext {
                home: home.clone(),
                runtime_dir: None,
                uid: users::get_current_uid(),
                protected: vec![],
            },
        )
        .unwrap();
        let machine = &config.machines.machines[0];
        let spec = serde_json::from_value(json!({"worker_id":"w", "machine":machine,
            "workspace":machine.workspaces[0].for_slot(1), "backend":"codex", "instructions":"", "model":"",
            "reasoning_effort":"", "job_timeout":30, "idle_timeout":30, "excluded_env":["OWNER_SECRET"], "slot":1})).unwrap();
        let backend = root.join("bin/codex");
        std::fs::write(&backend, BACKEND).unwrap();
        std::fs::set_permissions(&backend, std::fs::Permissions::from_mode(0o700)).unwrap();
        let env = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("HOME".into(), home.clone().into_os_string()),
            ("MODEL_AUTH".into(), "owner-model-auth".into()),
            ("FRIDICA_MCP_KEY".into(), "private-capability".into()),
            ("SLACK_TOKEN".into(), "xoxp-private".into()),
            ("OWNER_SECRET".into(), "owner-private".into()),
            ("DANGEROUS_ALIASES".into(), "[]".into()),
        ]);
        Self {
            _dir: dir,
            root,
            home,
            config,
            spec,
            env,
        }
    }
    fn write(&self, path: &str, data: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, data).unwrap();
    }
    fn launcher(&self) -> SystemLauncher {
        SystemLauncher::from_config(
            &self.config,
            self.home.clone(),
            self.env.clone(),
            self.root.join("private"),
        )
        .unwrap()
    }
    async fn run(&self) -> process::Completed {
        let launch = self
            .launcher()
            .launch(
                &self.spec,
                codex::command(&self.spec, &self.config.isolation.mcp_aliases),
            )
            .unwrap();
        process::run_with_open_stdin(launch, Duration::from_secs(10), 16384)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn local_startup_disables_discovered_registered_and_layered_aliases_before_initialization() {
    let mut f = Fixture::new();
    let source = "# owner comments stay intact\nmodel='owner-model'\n[mcp_servers.renamed]\ncommand='/opt/fridica'\nenv={KEY='private-source'}\n[mcp_servers.docs]\ncommand='docs-server'\n";
    f.write("home/.codex/config.toml", source);
    f.write(
        "home/.codex/review.config.toml",
        "[mcp_servers.layered]\ncommand='python3'\nargs=['-m','fridica','mcp']\n",
    );
    f.write("project/.codex/config.toml", "[mcp_servers.layered]\ncommand='opaque-wrapper'\nenabled=true\n[mcp_servers.web]\nurl='https://control.invalid/mcp?private=token'\n");
    f.write(
        "project/worker1/.codex/config.toml",
        "[mcp_servers.env_alias]\ncommand='wrapper'\nenv={Fridica_Control_Token='secret'}\n",
    );
    // Unrelated malformed Claude settings must not affect a Codex-only scan.
    f.write("home/.claude.json", "not JSON");
    f.config
        .isolation
        .mcp_aliases
        .push("registered.quoted\"alias".into());
    f.config
        .isolation
        .mcp_urls
        .push("https://control.invalid/mcp".into());
    f.env.insert(
        "DANGEROUS_ALIASES".into(),
        json!([
            "renamed",
            "layered",
            "web",
            "env_alias",
            "registered.quoted\"alias"
        ])
        .to_string()
        .into(),
    );
    let out = f.run().await;
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["cwd"], json!(f.spec.workspace.path));
    assert_eq!(value["unrestricted"], true);
    assert!(!String::from_utf8_lossy(&out.stdout).contains("private-source"));
    assert_eq!(
        std::fs::read_to_string(f.home.join(".codex/config.toml")).unwrap(),
        source
    );
    assert_eq!(
        std::fs::read_to_string(f.home.join("session-state")).unwrap(),
        "kept"
    );
    assert!(!f.spec.workspace.path.join("mcp-started").exists());
}

#[tokio::test]
async fn custom_codex_home_is_scanned_without_copying_or_replacing_authentication() {
    let mut f = Fixture::new();
    f.write(
        "custom/config.toml",
        "[mcp_servers.custom]\ncommand='fridica'\n",
    );
    f.write("custom/auth.json", "owner-authentication");
    f.env
        .insert("CODEX_HOME".into(), f.root.join("custom").into_os_string());
    f.env
        .insert("DANGEROUS_ALIASES".into(), "[\"custom\"]".into());
    let out = f.run().await;
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["codex_home"], json!(f.root.join("custom")));
    assert_eq!(
        std::fs::read_to_string(f.root.join("custom/auth.json")).unwrap(),
        "owner-authentication"
    );
    assert!(!f.home.join(".codex").exists());
}

#[tokio::test]
async fn malformed_linked_and_oversized_settings_refuse_before_backend_start_without_private_diagnostics(
) {
    for kind in [
        "malformed",
        "symlink",
        "hardlink",
        "oversized",
        "unsafe_home",
    ] {
        let mut f = Fixture::new();
        f.write("home/.codex/config.toml", "private invalid source");
        let settings = f.home.join(".codex/config.toml");
        match kind {
            "symlink" | "hardlink" => {
                std::fs::rename(&settings, f.root.join("original")).unwrap();
                if kind == "symlink" {
                    std::os::unix::fs::symlink(f.root.join("original"), &settings).unwrap();
                } else {
                    std::fs::hard_link(f.root.join("original"), &settings).unwrap();
                }
            }
            "oversized" => std::fs::write(&settings, "x".repeat(1024 * 1024 + 1)).unwrap(),
            "unsafe_home" => {
                f.env.insert("CODEX_HOME".into(), "relative/private".into());
            }
            _ => {}
        }
        let out = f.run().await;
        assert_eq!(out.returncode, 97, "{kind}");
        assert!(out.stdout.is_empty());
        assert_eq!(out.stderr, b"fridica worker MCP: setup refused\n");
        assert!(!f.home.join("session-state").exists());
        assert!(!f.spec.workspace.path.join("report.txt").exists());
    }
}

#[tokio::test]
async fn ssh_startup_scans_target_settings_without_a_confinement_inventory() {
    let mut f = Fixture::new();
    let remote = f.root.join("target-home");
    std::fs::create_dir(&remote).unwrap();
    f.write(
        "target-home/.codex/config.toml",
        "[mcp_servers.remote]\ncommand='fridica'\n",
    );
    f.write(
        "home/.codex/config.toml",
        "invalid local private config must not be read",
    );
    f.spec.machine.transport = "ssh".into();
    f.spec.machine.host = "owner@fixture".into();
    f.spec.workspace.path = "~/project/worker1".into();
    f.env
        .insert("DANGEROUS_ALIASES".into(), "[\"remote\"]".into());
    let ssh=format!("#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\ntest \"$1\" = owner@fixture || exit 255\nshift\nexport HOME={} FRIDICA_REMOTE_CAPABILITY=private OWNER_SECRET=private\nexec /bin/sh -c \"$1\"\n", shell::quote(remote.to_str().unwrap()));
    f.write("bin/ssh", &ssh);
    std::fs::set_permissions(
        f.root.join("bin/ssh"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        f.root.join("private"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let out = f.run().await;
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["home"], json!(remote));
    assert_eq!(value["cwd"], json!(remote.join("project/worker1")));
    assert!(!f.home.join("session-state").exists());
}

#[tokio::test]
async fn discovery_runs_at_each_process_start_and_does_not_cache_constructed_launch_settings() {
    let mut f = Fixture::new();
    f.write("home/.codex/config.toml", "model='owner-model'\n");
    f.env
        .insert("DANGEROUS_ALIASES".into(), "[\"late\"]".into());
    let launcher = f.launcher();
    let launch = launcher
        .launch(&f.spec, codex::command(&f.spec, &[]))
        .unwrap();
    // The launcher has been constructed, but the backend has not started yet.
    f.write(
        "home/.codex/config.toml",
        "[mcp_servers.late]\ncommand='fridica'\n",
    );
    let out = process::run_with_open_stdin(launch, Duration::from_secs(10), 16384)
        .await
        .unwrap();
    assert_eq!(
        out.returncode,
        0,
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::remove_file(f.home.join("session-state")).unwrap();
    f.write("home/.codex/config.toml", "invalid private-source");
    let launch = launcher
        .launch(&f.spec, codex::command(&f.spec, &[]))
        .unwrap();
    let out = process::run_with_open_stdin(launch, Duration::from_secs(10), 16384)
        .await
        .unwrap();
    assert_eq!(out.returncode, 97);
    assert!(!f.home.join("session-state").exists());
}
