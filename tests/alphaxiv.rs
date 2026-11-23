use fridica::{
    alphaxiv,
    config::{loader, LoadContext},
};

fn config() -> fridica::config::Config {
    let dir = tempfile::tempdir().unwrap();
    for name in ["home", "project", "private"] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
    }
    loader::parse(
        "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[scix]\nenabled=true\n[overleaf]\nenabled=true\n[alphaxiv]\nenabled=true\n[machines.local.policy]\ngpu_confine=false\nmode='write'\nnetwork=['api.alphaxiv.org']\n[machines.local.workspaces]\nproject='project'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'\n",
        &dir.path().join("config.toml"),
        &LoadContext { home: dir.path().join("home"), runtime_dir: None, uid: users::get_current_uid(), protected: vec![] },
    ).unwrap()
}

#[test]
fn research_tools_follow_configuration_and_network_policy() {
    let config = config();
    let mut policy = config.machines.get("local").unwrap().workspaces[0]
        .policy
        .clone();
    let servers = alphaxiv::servers(&config, "local", &policy);
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "alphaxiv");
    assert_eq!(servers[0].args, ["alphaxiv-mcp"]);
    assert_eq!(servers[0].env_vars, ["ALPHAXIV_TOKEN"]);
    assert!(!servers[0].auto_approve);
    assert!(!alphaxiv::available(&config, "missing", &policy));
    policy.network = vec!["alphaxiv.org".into()];
    assert!(!alphaxiv::available(&config, "local", &policy));
    policy.network = vec!["*.alphaxiv.org".into()];
    assert!(alphaxiv::available(&config, "local", &policy));
    policy.network.clear();
    assert!(!alphaxiv::available(&config, "local", &policy));
    policy.network = vec!["api.alphaxiv.org".into()];
    policy.fetch_repos = vec!["owner/repo".into()];
    assert!(!alphaxiv::available(&config, "local", &policy));
    policy.fetch_repos.clear();
    let mut disabled = config.clone();
    disabled.alphaxiv.enabled = false;
    assert!(alphaxiv::servers(&disabled, "local", &policy).is_empty());
}

#[test]
fn bundled_bridge_reports_missing_credentials_without_leaking_secrets() {
    for (command, token) in [
        ("alphaxiv-mcp", "ALPHAXIV_TOKEN"),
        ("scix-mcp", "ADS_TOKEN"),
        ("overleaf-mcp", "OVERLEAF_GIT_TOKEN"),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
            .args([command, "--probe"])
            .env_remove(token)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains(token));
    }
}

#[test]
fn ads_and_overleaf_follow_placement_policy() {
    use fridica::research::Service::{Overleaf, Scix};
    let config = config();
    let mut policy = config.machines.get("local").unwrap().workspaces[0]
        .policy
        .clone();
    policy.network = vec!["*.adsabs.harvard.edu".into()];
    policy.mode = "write".into();
    let ads = Scix.server(&config, "local", &policy).unwrap();
    assert_eq!(ads.args, ["scix-mcp"]);
    assert_eq!(ads.env_vars, ["ADS_TOKEN"]);
    assert!(!ads.auto_approve);
    assert!(!Overleaf.available(&config, "local", &policy));
    policy.mode = "read-only".into();
    assert!(!Scix.available(&config, "local", &policy));
    policy.mode = "full".into();
    policy.network = vec!["git.overleaf.com".into()];
    assert!(!Scix.available(&config, "local", &policy));
    assert!(!Overleaf.available(&config, "local", &policy));
    policy.network = vec!["*".into()];
    let paper = Overleaf.server(&config, "local", &policy).unwrap();
    assert_eq!(paper.args, ["overleaf-mcp"]);
    assert!(paper.env_vars.iter().any(|v| v == "OVERLEAF_GIT_TOKEN"));
    assert!(!paper.auto_approve);
    policy.mode = "write".into();
    let workspace = config.machines.get("local").unwrap().workspaces[0].for_slot(2);
    let server = Overleaf
        .server_at(&config, "local", &policy, Some(&workspace.path))
        .unwrap();
    assert_eq!(
        server.args,
        [
            "overleaf-mcp",
            "--workspace-write",
            workspace.path.to_str().unwrap()
        ]
    );
    assert!(Overleaf.server(&config, "local", &policy).is_none());
    policy.mode = "read-only".into();
    assert!(!Overleaf.available(&config, "local", &policy));
    policy.mode = "write".into();
    policy.fetch_repos.push("owner/project".into());
    for service in [Scix, Overleaf] {
        assert!(!service.available(&config, "local", &policy));
    }
}

#[test]
fn bundled_ads_and_overleaf_serve_tools_over_stdio() {
    use serde_json::{json, Value};
    use std::{
        io::{BufRead, BufReader, Write},
        process::{Command, Stdio},
    };
    for (command, expected) in [("scix-mcp", "search"), ("overleaf-mcp", "pull")] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fridica"))
            .arg(command)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("ADS_TOKEN", "fixture")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut replies = BufReader::new(child.stdout.take().unwrap()).lines();
        let init = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"1"}}});
        writeln!(input, "{init}").unwrap();
        let reply: Value = serde_json::from_str(&replies.next().unwrap().unwrap()).unwrap();
        assert!(reply["result"]["serverInfo"].is_object(), "{reply}");
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list"})
        )
        .unwrap();
        let reply: Value = serde_json::from_str(&replies.next().unwrap().unwrap()).unwrap();
        assert!(
            reply["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["name"] == expected),
            "{reply}"
        );
        drop(input);
        assert!(child.wait().unwrap().success());
    }
}

#[test]
fn write_mode_overleaf_confines_git_checkouts_to_the_assigned_directory() {
    use serde_json::{json, Value};
    use std::{
        fs,
        io::Write,
        os::unix::fs::symlink,
        process::{Command, Stdio},
    };
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().canonicalize().unwrap();
    let slot = base.join("worker 2");
    let remote = base.join("origin");
    let outside = base.join("other worker");
    for path in [&slot, &remote, &outside] {
        fs::create_dir(path).unwrap();
    }
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=user@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "initial",
        ],
    ] {
        assert!(Command::new("git")
            .current_dir(&remote)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
    symlink(&outside, slot.join("elsewhere")).unwrap();
    let config = base.join("interleaf.toml");
    fs::write(
        &config,
        format!(
            "[projects.paper]\ngit_url = {}\n[projects.sibling]\ngit_url = {}\ndir = {}\n",
            json!(remote),
            json!(remote),
            json!(slot.join("elsewhere/paper"))
        ),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_fridica"))
        .arg("overleaf-mcp")
        .arg("--workspace-write")
        .arg(&slot)
        .current_dir(&outside)
        .env("INTERLEAF_WORKSPACE", &outside)
        .env("INTERLEAF_CONFIG", config)
        .env("HOME", &base)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for (i, project) in ["paper", "sibling"].into_iter().enumerate() {
        writeln!(input, "{}", json!({"jsonrpc":"2.0","id":i+1,"method":"tools/call","params":{"name":"pull","arguments":{"project":project}}})).unwrap();
    }
    drop(input);
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let rows: Vec<Value> = String::from_utf8(result.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(slot.join("paper/.git").is_dir(), "{rows:?}");
    assert_ne!(rows[0]["result"]["isError"], true, "{rows:?}");
    assert_eq!(rows[1]["result"]["isError"], true, "{rows:?}");
    assert!(!outside.join("paper").exists());
}
