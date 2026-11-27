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
    let mut legacy_refusals_missing = vec![];
    for case in [
        "automatic",
        "automatic_project",
        "missing",
        "missing_channel",
        "scope",
        "project",
        "saved_project",
        "worker",
        "machine",
        "workspace",
        "expired",
        "channel",
        "allowed",
    ] {
        let dir = tempfile::tempdir().unwrap();
        // This directory is also the SSH control directory, independent of umask.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("work")).unwrap();
        std::fs::create_dir(root.join("bin")).unwrap();
        let source = "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CEXAMPLE']\n[overleaf]\nenabled=true\nnative_adapter=['obsolete-adapter']\n[machines.dungeon3]\ntransport='ssh'\nhost='fixture'\n[machines.dungeon3.policy]\nmode='full'\nnetwork=['*']\ngpu_confine=false\n[machines.dungeon3.workspaces]\nsnapy='work'\n";
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
            project: Some(PROJECT.into()),
            channel: Some(CHANNEL.into()),
        });
        if case == "scope" {
            config.overleaf.native_scope = None;
        }
        if case == "project" {
            config.overleaf.native_scope.as_mut().unwrap().project = Some("4".repeat(24));
        }
        if case == "missing_channel" {
            config.overleaf.native_scope.as_mut().unwrap().channel = None;
        }
        let recipient = Recipient {
            worker_id: WORKER.into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        if !matches!(case, "missing" | "automatic" | "automatic_project") {
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
        server.env_vars = vec!["OVERLEAF_NATIVE_SESSION".into()];
        server.args = vec![
            "overleaf-mcp".into(),
            "--workspace-write".into(),
            root.join("work").to_string_lossy().into_owned(),
            "--native-channel".into(),
            CHANNEL.into(),
            "--native-worker".into(),
            WORKER.into(),
        ];
        if matches!(case, "automatic" | "automatic_project") {
            let scope = config.overleaf.native_scope.as_mut().unwrap();
            if case == "automatic" {
                scope.project = None;
            }
            scope.channel = None;
        }
        server
            .args
            .extend(["--thread-project".into(), PROJECT.into()]);
        let machine = &config.machines.machines[0];
        let mut spec: WorkerSpec = serde_json::from_value(json!({"worker_id":WORKER,"machine":machine,
            "workspace":machine.workspaces[0],"mcp_servers":[server],"backend":"claude","instructions":"","model":"","reasoning_effort":"",
            "job_timeout":10,"idle_timeout":10,"excluded_env":[],"slot":0,"thread_project":PROJECT,"native_channel": if case == "channel" { "COTHER" } else { CHANNEL }})).unwrap();
        match case {
            "worker" => spec.worker_id = "worker-22222222-2222-4222-8222-222222222222".into(),
            "machine" => spec.machine.name = "different".into(),
            "workspace" => spec.workspace.name = "different".into(),
            _ => {}
        }
        let fake = root.join("bin/ssh");
        std::fs::write(&fake,format!("#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift\nshift\nexec /usr/bin/env -i PATH={target_path}:/usr/bin:/bin OVERLEAF_NATIVE_SESSION=fixture-target-ambient /bin/sh -c \"$1\"\n", target_path=std::path::Path::new(env!("CARGO_BIN_EXE_fridica")).parent().unwrap().display())).unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut saved: serde_json::Value = serde_json::from_str(&envelope()).unwrap();
        if matches!(case, "automatic" | "automatic_project") {
            saved["schema"] = 3.into();
            if case == "automatic" {
                saved.as_object_mut().unwrap().remove("projects");
            }
        }
        if case == "saved_project" {
            saved["projects"] = json!(["4".repeat(24)]);
        }
        let environment = BTreeMap::from([
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin", root.join("bin").display()).into(),
            ),
            ("OVERLEAF_GIT_TOKEN".into(), "fixture-git".into()),
            ("ADS_TOKEN".into(), "fixture-ads".into()),
            ("OVERLEAF_NATIVE_SESSION".into(), saved.to_string().into()),
        ]);
        let launcher =
            SystemLauncher::from_config(&config, root.clone(), environment, root.clone()).unwrap();
        let launch_result = launcher.launch(&spec,vec!["/usr/bin/python3".into(),"-c".into(),
            "import os,json; print(json.dumps({'native': 'OVERLEAF_NATIVE_SESSION' in os.environ, 'git':os.environ.get('OVERLEAF_GIT_TOKEN'), 'cookie_exposed': 'overleaf_session2=' in os.environ.get('OVERLEAF_NATIVE_SESSION',''), 'broker':os.environ.get('OVERLEAF_NATIVE_SESSION','').startswith('unix:')}))".into()]);

        if case == "saved_project" {
            assert!(launch_result.is_err());
            continue;
        }
        if matches!(case, "missing" | "missing_channel") {
            if launch_result.is_ok() {
                legacy_refusals_missing.push(case);
            }
            continue;
        }
        let mut launch = launch_result.unwrap();
        assert_eq!(
            launch
                .env
                .contains_key(OsStr::new("OVERLEAF_NATIVE_SESSION")),
            matches!(case, "allowed" | "automatic" | "automatic_project")
        );
        assert!(!launch.argv.iter().any(|a| a.contains("fixture-native")));
        if let Some(raw) = launch.env.get(OsStr::new("OVERLEAF_NATIVE_SESSION")) {
            let grant: serde_json::Value = serde_json::from_str(raw.to_str().unwrap()).unwrap();
            assert_eq!(grant["recipient"]["worker_id"], WORKER);
            assert_eq!(grant["channel"], CHANNEL);
            assert_eq!(grant["projects"], json!([PROJECT]));
        }
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
        assert_eq!(
            result["native"],
            matches!(case, "allowed" | "automatic" | "automatic_project"),
            "{case}"
        );
        assert_eq!(result["git"], "fixture-git");
        assert_eq!(result["cookie_exposed"], false, "{case}");
        assert_eq!(
            result["broker"],
            matches!(case, "allowed" | "automatic" | "automatic_project")
        );
    }
    assert!(
        legacy_refusals_missing.is_empty(),
        "legacy sessions forwarded without grants: {legacy_refusals_missing:?}"
    );
}

#[test]
fn standalone_login_prepares_without_node_backend_store_or_browser() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        "# keep owner comments\n[overleaf]\nenabled=false\n",
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_fridica");
    let invoke = || {
        let mut cmd = std::process::Command::new(binary);
        cmd.env("PATH", root.path())
            .args(["overleaf-auth", "login", "--prepare-only", "--config"])
            .arg(&config)
            .args(["--expected-email", "Owner@example.invalid"]);
        cmd
    };
    assert!(invoke().status().unwrap().success());
    let source = std::fs::read_to_string(&config).unwrap();
    assert!(source.starts_with("# keep owner comments"));
    assert!(source.contains("owner@example.invalid"));
    assert!(!source.contains("native_recipient"));
    assert!(!source.contains("project ="));
    assert!(!source.contains("channel ="));
    assert!(!source.contains("resources"));
    assert!(!source.contains("node"));
    assert!(!root.path().join("overleaf-login").exists());
    assert_eq!(
        std::fs::metadata(&config).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(invoke().status().unwrap().success());
    assert_eq!(source, std::fs::read_to_string(config).unwrap());
}
