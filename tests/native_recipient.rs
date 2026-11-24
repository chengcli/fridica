//! Synthetic owner configuration and launches only: never access a real secret store.
use fridica::{
    config::{loader, overleaf::Recipient, LoadContext},
    exec::{isolation, process},
    workers::{
        launcher::{Launcher, SystemLauncher},
        protocol::WorkerSpec,
    },
};
use serde_json::json;
use std::{collections::BTreeMap, ffi::OsStr, os::unix::fs::PermissionsExt, time::Duration};
const CHANNEL: &str = "CEXAMPLE";
const PROJECT: &str = "333333333333333333333333";
const WORKER: &str = "worker-11111111-1111-4111-8111-111111111111";
fn envelope() -> String {
    json!({"schema":2,"cookie":"overleaf_session2=fixture-native", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":[PROJECT], "operations":fridica::config::overleaf::OPERATIONS,"expires_at":null}).to_string()
}

#[tokio::test]
async fn native_recipient_launch_gate_matches_actual_identity_and_strips_remote_ambient() {
    for case in [
        "missing",
        "scope",
        "project",
        "worker",
        "machine",
        "workspace",
        "expired",
        "channel",
        "allowed",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        let source = "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CEXAMPLE']\n[overleaf]\nenabled=true\nnative_adapter=['python3','/fixture/adapter.py']\n[machines.dungeon3]\ntransport='ssh'\nhost='fixture'\n[machines.dungeon3.policy]\nmode='full'\ngpu_confine=false\n[machines.dungeon3.workspaces]\nsnapy='work'\n";
        let context = LoadContext {
            home: root.clone(),
            runtime_dir: None,
            uid: users::get_current_uid(),
            protected: vec![],
        };
        let mut config = loader::parse(
            &source.replace(
                "snapy='work'",
                &format!("snapy='{}'", root.join("work").display()),
            ),
            &root.join("config.toml"),
            &context,
        )
        .unwrap();
        config.overleaf.native_scope = Some(fridica::config::overleaf::Scope {
            expected_email: "owner@example.invalid".into(),
            project: PROJECT.into(),
            channel: Some(CHANNEL.into()),
        });
        if case == "scope" {
            config.overleaf.native_scope = None;
        }
        if case == "project" {
            config.overleaf.native_scope.as_mut().unwrap().project = "4".repeat(24);
        }
        let recipient = Recipient {
            worker_id: WORKER.into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        if case != "missing" {
            config.overleaf.native_recipient = Some(recipient);
        }
        if case == "expired" {
            config
                .overleaf
                .native_recipient
                .as_mut()
                .unwrap()
                .expires_at = Some(1);
        }
        let mut server = fridica_agent::overleaf::overleaf("fridica");
        // Simulate even a stale/requested native profile: the launch gate is authoritative.
        server.env_vars.push("OVERLEAF_NATIVE_SESSION".into());
        server.args = vec![
            "--channel".into(),
            if case == "channel" {
                "OTHER".into()
            } else {
                CHANNEL.into()
            },
            "--worker".into(),
            WORKER.into(),
        ];
        let machine = &config.machines.machines[0];
        let mut spec: WorkerSpec = serde_json::from_value(json!({"worker_id":WORKER,"machine":machine,
            "workspace":machine.workspaces[0],"mcp_servers":[server],"backend":"claude","instructions":"","model":"","reasoning_effort":"",
            "job_timeout":10,"idle_timeout":10,"excluded_env":[],"slot":0})).unwrap();
        match case {
            "worker" => spec.worker_id = "worker-22222222-2222-4222-8222-222222222222".into(),
            "machine" => spec.machine.name = "different".into(),
            "workspace" => spec.workspace.name = "different".into(),
            _ => {}
        }
        let fake = root.join("bin/ssh");
        std::fs::write(&fake,"#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nshift\nexec /usr/bin/env -i PATH=/usr/bin:/bin OVERLEAF_NATIVE_SESSION=fixture-target-ambient /bin/sh -c \"$1\"\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("OVERLEAF_GIT_TOKEN".into(), "fixture-git".into()),
            ("ADS_TOKEN".into(), "fixture-ads".into()),
            ("OVERLEAF_NATIVE_SESSION".into(), envelope().into()),
        ]);
        let launcher =
            SystemLauncher::from_config(&config, root.clone(), environment, root.clone()).unwrap();
        let launch_result = launcher.launch(&spec,vec!["/usr/bin/python3".into(),"-c".into(),
            "import os,json; print(json.dumps({'native': 'OVERLEAF_NATIVE_SESSION' in os.environ, 'git':os.environ.get('OVERLEAF_GIT_TOKEN')}))".into()]);
        if case == "project" {
            assert!(launch_result.is_err());
            continue;
        }
        let mut launch = launch_result.unwrap();
        assert_eq!(
            launch
                .env
                .contains_key(OsStr::new("OVERLEAF_NATIVE_SESSION")),
            case == "allowed"
        );
        assert!(!launch.argv.iter().any(|a| a.contains("fixture-native")));
        isolation::read_only_ssh_probe(&mut launch);
        let output = process::run_once(launch, vec![], Duration::from_secs(10), 4096)
            .await
            .unwrap();
        assert_eq!(
            output.returncode,
            0,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["native"], case == "allowed", "{case}");
        assert_eq!(result["git"], "fixture-git");
    }
}

#[test]
fn standalone_login_prepares_scope_and_resources_without_backend_or_worker() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "# keep owner comments\n[overleaf]\nenabled=false\n",
    )
    .unwrap();
    let resources = root.path().join("bundle");
    let upstream = resources.join("node_modules/@netique/overleaf-mcp/dist/auth");
    std::fs::create_dir_all(&upstream).unwrap();
    std::fs::write(upstream.join("browserLogin.js"), "fixture").unwrap();
    let node = root.path().join("node");
    // Resource check itself is exercised by the real Node helper smoke. This
    // fixture proves CLI dispatch/config preparation without a backend on PATH.
    std::fs::write(
        &node,
        "#!/bin/sh\ncase \"$*\" in *--check*) exit 0;; *) exit 1;; esac\n",
    )
    .unwrap();
    std::fs::set_permissions(&node, std::fs::Permissions::from_mode(0o700)).unwrap();
    let binary = env!("CARGO_BIN_EXE_fridica");
    let invoke = || {
        let mut cmd = std::process::Command::new(binary);
        cmd.env("PATH", root.path())
            .args(["overleaf-auth", "login", "--prepare-only", "--config"])
            .arg(&config)
            .args([
                "--expected-email",
                "Owner@example.invalid",
                "--project",
                PROJECT,
                "--resources",
            ])
            .arg(&resources)
            .arg("--node")
            .arg(&node);
        cmd
    };
    assert!(invoke().status().unwrap().success());
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(source.starts_with("# keep owner comments"));
    assert!(source.contains("owner@example.invalid"));
    assert!(!source.contains("native_recipient"));
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(invoke().status().unwrap().success());
    assert_eq!(source, std::fs::read_to_string(config).unwrap());
}
