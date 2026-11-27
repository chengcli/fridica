//! Explicit GitHub MCP configuration. Workers resolve credentials from their
//! environment (including forwarded integration tokens) or target-local gh auth.
//! Configuration, commands and replay contain names only.
use crate::config::{registry::Policy, Config};

pub use crate::config::github_mcp::{Settings, Target};
pub const HELPER: &str = include_str!("mcp_helper.py");
pub const SERVER_NAME: &str = "fridica_github";

/// No server for offline or scoped-fetch-only placements. The backend's shell
/// network settings do not constrain MCP subprocesses, so gate them here too.
pub fn target(config: &Config, machine: &str, policy: &Policy) -> Option<Target> {
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
    Some(
        config
            .github
            .mcp
            .machines
            .get(machine)
            .cloned()
            .unwrap_or_default(),
    )
}
pub fn available(config: &Config) -> bool {
    config.machines.machines.iter().any(|m| {
        m.workspaces
            .iter()
            .any(|w| target(config, &m.name, &w.policy).is_some())
    })
}
pub fn servers(config: &Config, machine: &str, policy: &Policy) -> Vec<fridica_agent::McpServer> {
    target(config, machine, policy)
        .map(|target| {
            let mut server = server(&target, policy.mode == "read-only");
            if target.command.is_empty()
                && config
                    .machines
                    .get(machine)
                    .is_some_and(|m| m.transport == "local")
            {
                if let Ok(exe) = std::env::current_exe() {
                    if exe
                        .file_name()
                        .is_some_and(|n| n == "fridica" || n == "fridica-candidate")
                    {
                        server.command = exe.to_string_lossy().into_owned();
                    }
                }
            }
            vec![server]
        })
        .unwrap_or_default()
}
pub fn server(target: &Target, readonly: bool) -> fridica_agent::McpServer {
    if target.command.is_empty() {
        let mut args = vec![
            "github-mcp".into(),
            "--token-env".into(),
            target.token_env.clone(),
        ];
        if readonly {
            args.push("--read-only".into());
        }
        return fridica_agent::McpServer {
            name: SERVER_NAME.into(),
            command: "fridica".into(),
            args,
            env_vars: vec![target.token_env.clone(), "GH_CONFIG_DIR".into()],
            auto_approve: true,
        };
    }
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
pub fn configured_probe_command(config: &Config, machine: &str, policy: &Policy) -> Vec<String> {
    let server = servers(config, machine, policy).remove(0);
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
    probe_with_control(config, home, environment, machine, workspace, timeout, None).await
}
pub(crate) async fn probe_with_control(
    config: &Config,
    home: &std::path::Path,
    environment: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    machine: &crate::config::registry::Machine,
    workspace: &crate::config::registry::Workspace,
    timeout: std::time::Duration,
    shared_control: Option<&std::path::Path>,
) -> anyhow::Result<bool> {
    let Some(server) = servers(config, &machine.name, &workspace.policy).pop() else {
        return Ok(true);
    };
    let (launch, _control) = crate::workers::mcp_probe::launch(
        config,
        home,
        environment,
        machine,
        workspace,
        server,
        shared_control,
    )?;
    Ok(probe_launch(launch, timeout).await)
}
pub async fn probe_launch(
    launch: crate::exec::process::Launch,
    timeout: std::time::Duration,
) -> bool {
    crate::exec::probe::run(launch, timeout)
        .await
        .is_ok_and(|r| r.returncode == 0 && r.stdout == b"fridica-github-mcp:ready\n")
}
