//! Explicit target checks. Reports contain fixed diagnostics, never subprocess
//! output, credentials, inventory paths, or MCP settings.
use crate::{
    config::{Config, LoadContext},
    exec::{
        isolation::Isolation,
        local::LocalTransport,
        process::{self, Launch},
        ssh::SshTransport,
    },
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    Passed,
    MissingRemoteInventory,
    UnsupportedTransport,
    LaunchConfigurationRefused,
    InventoryRefused,
    SettingsRefused,
    NamespaceFailed,
    RuntimeOrTransportFailed,
    ProbeFailed,
}
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

/// Bounded process ownership includes cancellation cleanup and the SSH stdin
/// watchdog. Exact markers and successful exit are both required for success.
pub async fn run_probe(launch: Launch, timeout: Duration) -> Check {
    let Ok(result) = process::run_with_open_stdin(launch, timeout, 4096).await else {
        return Check::ProbeFailed;
    };
    match (result.returncode, result.stdout.as_slice()) {
        (0, b"fridica-isolation:namespace\nfridica-isolation:ready\n") => Check::Passed,
        (97, b"fridica-isolation:inventory-refused\n") => Check::InventoryRefused,
        (97, b"fridica-isolation:settings-refused\n") => Check::SettingsRefused,
        (_, b"fridica-isolation:namespace\n")
        | (97, b"fridica-isolation:namespace\nfridica-isolation:namespace-refused\n") => {
            Check::NamespaceFailed
        }
        _ => Check::RuntimeOrTransportFailed,
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
        "local" => isolation.preflight(
            &LocalTransport {
                machine: machine.clone(),
                home: context.home.clone(),
                excluded_env,
            },
            &workspace.path,
            environment,
        ),
        "ssh" => {
            control = Some(
                tempfile::Builder::new()
                    .prefix("fridica-probe-")
                    .permissions(std::fs::Permissions::from_mode(0o700))
                    .tempdir()?,
            );
            let mut launch = isolation.preflight_remote(
                &SshTransport {
                    machine: machine.clone(),
                    excluded_env,
                    control_directory: control.as_ref().unwrap().path().to_owned(),
                },
                &workspace.path.to_string_lossy(),
                environment,
            );
            if let Ok(launch) = &mut launch {
                // First value wins in OpenSSH. A check must not reuse a live
                // master, leave a persistent master, or enroll a new host key.
                launch.argv.splice(
                    1..1,
                    [
                        "-o",
                        "ControlMaster=no",
                        "-o",
                        "ControlPath=none",
                        "-o",
                        "ControlPersist=no",
                        "-o",
                        "StrictHostKeyChecking=yes",
                        "-o",
                        "UpdateHostKeys=no",
                    ]
                    .map(str::to_owned),
                );
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
