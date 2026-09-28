//! Explicit target checks. Reports contain fixed diagnostics, never subprocess
//! output, credentials, inventory paths, or MCP settings.
pub mod readiness;

use crate::{
    config::{Config, LoadContext},
    exec::{isolation::Isolation, local::LocalTransport, ssh::SshTransport},
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, time::Duration};

pub use crate::exec::isolation::{run_probe, Check};
#[derive(Debug, Serialize)]
pub struct Report {
    pub machine: String,
    pub workspace: String,
    pub check: Check,
    // A successful mount probe is not backend conformance or active parity.
    pub active_launch_ready: bool,
}
impl Report {
    pub fn passed(&self) -> bool {
        self.check == Check::Passed
    }
}

/// Check only the explicitly selected machine/workspace. The inventory remains
/// an owner assertion: this cannot discover arbitrary secret copies or custom
/// backend configuration sources. No database, backend, or API is opened.
pub async fn isolation(
    config: &Config,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    machine: &str,
    workspace: &str,
    timeout: Duration,
) -> Result<Report> {
    isolation_backend(
        config,
        context,
        environment,
        machine,
        workspace,
        timeout,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn isolation_backend(
    config: &Config,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    machine: &str,
    workspace: &str,
    timeout: Duration,
    backend: Option<&str>,
) -> Result<Report> {
    if timeout < Duration::from_secs(1) || timeout > Duration::from_secs(120) {
        bail!("isolation probe timeout must be between 1 and 120 seconds");
    }
    let machine = config.machines.get(machine).context("unknown machine")?;
    let workspace = machine.workspace(workspace).context("unknown workspace")?;
    let mut report = Report {
        machine: machine.name.clone(),
        workspace: workspace.name.clone(),
        check: Check::LaunchConfigurationRefused,
        active_launch_ready: false,
    };
    if machine.transport == "ssh" && !config.isolation.remote.contains_key(&machine.name) {
        report.check = Check::MissingRemoteInventory;
        return Ok(report);
    }
    let isolation = Isolation::new(config, &[])?;
    let excluded_env = vec![
        config.slack.app_token_env.clone(),
        config.slack.user_token_env.clone(),
    ];
    // Retain the private temporary directory until SSH and its cleanup exit.
    let mut control = None;
    let launch = match machine.transport.as_str() {
        "local" => {
            let probe = |transport: &LocalTransport, path: &std::path::Path, environment| {
                if let Some(backend) = backend {
                    isolation.preflight_backend(transport, path, environment, backend)
                } else {
                    isolation.preflight(transport, path, environment)
                }
            };
            probe(
                &LocalTransport {
                    machine: machine.clone(),
                    home: context.home.clone(),
                    excluded_env,
                },
                &workspace.path,
                environment,
            )
        }
        "ssh" => {
            control = Some(
                tempfile::Builder::new()
                    .prefix("fridica-probe-")
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir()?,
            );
            let probe = |transport: &SshTransport, path: &str, environment| {
                if let Some(backend) = backend {
                    isolation.preflight_backend_remote(transport, path, environment, backend)
                } else {
                    isolation.preflight_remote(transport, path, environment)
                }
            };
            let mut launch = probe(
                &SshTransport {
                    machine: machine.clone(),
                    excluded_env,
                    control_directory: control.as_ref().unwrap().path().to_owned(),
                },
                &workspace.path.to_string_lossy(),
                environment,
            );
            if let Ok(launch) = &mut launch {
                crate::exec::isolation::read_only_ssh_probe(launch);
            }
            launch
        }
        _ => {
            report.check = Check::UnsupportedTransport;
            return Ok(report);
        }
    };
    if let Ok(launch) = launch {
        report.check = run_probe(launch, timeout).await;
    }
    drop(control);
    Ok(report)
}
