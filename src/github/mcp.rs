//! Explicit, target-local GitHub MCP configuration. Credentials are resolved only
//! by the child helper; configuration, commands and replay contain names only.
use crate::config::{registry::Policy, Config};
use std::os::unix::fs::PermissionsExt;

pub use crate::config::github_mcp::{Settings, Target};
pub const HELPER: &str = include_str!("mcp_helper.py");
pub const SERVER_NAME: &str = "fridica_github";

/// No server for offline or scoped-fetch-only placements. The backend's shell
/// network settings do not constrain MCP subprocesses, so gate them here too.
pub fn target<'a>(config: &'a Config, machine: &str, policy: &Policy) -> Option<&'a Target> {
    let transport = &config.machines.get(machine)?.transport;
    let network = policy.any_network()
        || policy.network.iter().any(|domain| {
            let domain = domain.to_ascii_lowercase();
            matches!(domain.as_str(), "github.com" | "api.github.com")
                || domain
                    .strip_prefix("*.")
                    .is_some_and(|suffix| "api.github.com".ends_with(&format!(".{suffix}")))
        });
    if !config.github.mcp.enabled
        || !policy.fetch_repos.is_empty()
        || !network
        || !matches!(transport.as_str(), "local" | "ssh")
    {
        return None;
    }
    config.github.mcp.machines.get(machine)
}
pub fn servers(config: &Config, machine: &str, policy: &Policy) -> Vec<fridica_agent::McpServer> {
    target(config, machine, policy)
        .map(|target| vec![server(target, policy.mode == "read-only")])
        .unwrap_or_default()
}
pub fn server(target: &Target, readonly: bool) -> fridica_agent::McpServer {
    fridica_agent::McpServer {
        name: SERVER_NAME.into(), command: "/usr/bin/python3".into(),
        args: vec!["-I".into(), "-S".into(), "-c".into(), HELPER.into(),
            serde_json::json!({"command":target.command,"token_env":target.token_env,"readonly":readonly}).to_string()],
        env_vars: vec![target.token_env.clone(), "GH_CONFIG_DIR".into()],
        auto_approve: true,
    }
}
pub fn probe_command(target: &Target, readonly: bool) -> Vec<String> {
    let server = server(target, readonly);
    let mut command = vec![server.command];
    command.extend(server.args);
    command.push("--probe".into());
    command
}

/// The same target-local launch path as a worker, including SSH cleanup and
/// confinement. Probes never call a GitHub write tool.
pub async fn probe(
    config: &Config,
    home: &std::path::Path,
    environment: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    machine: &crate::config::registry::Machine,
    workspace: &crate::config::registry::Workspace,
    timeout: std::time::Duration,
) -> anyhow::Result<bool> {
    use crate::workers::jsonl::{Launcher, SystemLauncher};
    let Some(target) = target(config, &machine.name, &workspace.policy) else {
        return Ok(true);
    };
    let control = tempfile::Builder::new()
        .prefix("fridica-mcp-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let launcher = SystemLauncher::from_config(
        config,
        home.to_owned(),
        environment,
        control.path().to_owned(),
    )?;
    let spec = crate::workers::protocol::WorkerSpec {
        mcp_servers: servers(config, &machine.name, &workspace.policy),
        worker_id: String::new(),
        machine: machine.clone(),
        workspace: workspace.clone(),
        backend: "codex".into(),
        instructions: String::new(),
        model: String::new(),
        reasoning_effort: String::new(),
        job_timeout: 30.,
        idle_timeout: 30.,
        excluded_env: config.secret_env().iter().map(|s| s.to_string()).collect(),
        slot: 0,
    };
    let mut launch = launcher
        .launch(
            &spec,
            probe_command(target, workspace.policy.mode == "read-only"),
        )
        .map_err(|_| anyhow::anyhow!("GitHub MCP launch refused"))?;
    if machine.transport == "ssh" {
        crate::exec::isolation::read_only_ssh_probe(&mut launch);
    }
    Ok(probe_launch(launch, timeout).await)
}
pub async fn probe_launch(
    launch: crate::exec::process::Launch,
    timeout: std::time::Duration,
) -> bool {
    crate::exec::process::run_with_open_stdin(launch, timeout, 4096)
        .await
        .is_ok_and(|r| r.returncode == 0 && r.stdout == b"fridica-github-mcp:ready\n")
}
