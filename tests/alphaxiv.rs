use fridica::{
    alphaxiv,
    config::{loader, LoadContext},
};

#[test]
fn research_tools_follow_configuration_and_network_policy() {
    let dir = tempfile::tempdir().unwrap();
    let config = loader::parse(
        "[owner]\nslack_user='UOWNER'\n[slack]\nworkspace='TTEAM'\nchannels=['CROOM']\n[alphaxiv]\nenabled=true\n[machines.local.policy]\ngpu_confine=false\nmode='write'\nnetwork=['api.alphaxiv.org']\n[machines.local.workspaces]\nproject='project'\n[state]\npath='private/db'\ncontrol_socket='private/control.sock'\n",
        &dir.path().join("config.toml"),
        &LoadContext { home: dir.path().join("home"), runtime_dir: None, uid: users::get_current_uid(), protected: vec![] },
    ).unwrap();
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
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_fridica"))
        .args(["alphaxiv-mcp", "--probe"])
        .env_remove("ALPHAXIV_TOKEN")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ALPHAXIV_TOKEN"));
}
