//! Launch an MCP server's diagnostic command with the worker's environment.
use crate::{
    config::{
        registry::{Machine, Workspace},
        Config,
    },
    workers::launcher::{Launcher, SystemLauncher},
};
use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::Path};

pub(crate) fn launch(
    config: &Config,
    home: &Path,
    environment: BTreeMap<OsString, OsString>,
    machine: &Machine,
    workspace: &Workspace,
    server: fridica_agent::McpServer,
) -> anyhow::Result<(crate::exec::process::Launch, tempfile::TempDir)> {
    let mut command = vec![server.command.clone()];
    command.extend(server.args.iter().cloned());
    command.push("--probe".into());
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
        python_env: config
            .python_envs
            .get(&machine.name)
            .cloned()
            .unwrap_or_default(),
        mcp_servers: vec![server],
        native_channel: None,
        thread_project: None,
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
        .launch(&spec, command)
        .map_err(|_| anyhow::anyhow!("MCP probe launch refused"))?;
    if machine.transport == "ssh" {
        crate::exec::isolation::read_only_ssh_probe(&mut launch);
    }
    Ok((launch, control))
}
