//! Startup preparation only. Never opens durable state, reads token values,
//! invokes backends, or certifies the later rollout gates.
use super::{isolation_backend, Check};
use crate::{
    config::{registry::Machine, Config, LoadContext},
    exec::{
        isolation::{self as helpers, Isolation},
        local::LocalTransport,
        ssh::{LaunchOptions, SshTransport},
    },
};
use anyhow::{bail, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::Path, time::Duration,
};
use tokio::sync::watch;

#[derive(Debug, Serialize)]
pub struct Target {
    pub machine: String,
    pub workspace: String,
    pub backend: String,
    pub check: Check,
    pub isolation: Option<Check>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub build_version: &'static str,
    pub config_fingerprint: String,
    pub host: Check,
    pub owner_inputs: Check,
    pub parent_backend: String,
    pub parent: Check,
    pub targets: Vec<Target>,
    pub cancelled: bool,
    pub startup_checks_passed: bool,
}

/// Check all configured worker destinations. Completion of an in-flight probe
/// includes cleanup; stop is observed before the next target/phase, so shutdown
/// waits for at most the current bounded probe, never every remaining target.
pub async fn check(
    config: &Config,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    stop: watch::Receiver<bool>,
) -> Result<Report> {
    if !(Duration::from_secs(1)..=Duration::from_secs(120)).contains(&timeout) {
        bail!("readiness timeout must be between 1 and 120 seconds");
    }
    let boundary = Isolation::new(config, &[])?;
    let mut report = Report {
        build_version: env!("CARGO_PKG_VERSION"),
        config_fingerprint: config.fingerprint.clone(),
        host: Check::Passed,
        owner_inputs: Check::Passed,
        parent_backend: config.parent.backend.clone(),
        parent: Check::ProbeFailed,
        targets: vec![],
        cancelled: *stop.borrow(),
        startup_checks_passed: false,
    };
    if report.cancelled {
        return Ok(report);
    }
    if config.state.control_socket.parent().is_none_or(|parent| {
        crate::control::server::validate_parent_directory(parent, true).is_err()
    }) {
        report.host = Check::HostPathsRefused;
    }
    if crate::config::contract::load(config.owner.contract.as_deref()).is_err()
        || crate::config::repos::load(config.parent.repos.as_deref()).is_err()
    {
        report.owner_inputs = Check::OwnerInputsRefused;
    }
    if report.host != Check::Passed || report.owner_inputs != Check::Passed {
        return Ok(report);
    }
    let machine = Machine {
        transport: "local".into(),
        ..config.machines.machines[0].clone()
    };
    report.parent = probe(
        &boundary,
        config,
        context,
        environment.clone(),
        &machine,
        Path::new("/"),
        &config.parent.backend,
        true,
        timeout,
    )
    .await;
    for machine in &config.machines.machines {
        for workspace in &machine.workspaces {
            if *stop.borrow() {
                report.cancelled = true;
                break;
            }
            let supported = matches!(machine.transport.as_str(), "local" | "ssh");
            for backend in &machine.backends {
                if *stop.borrow() {
                    report.cancelled = true;
                    break;
                }
                let isolation = if supported && workspace.policy.gpu_confine == Some(true) {
                    Some(
                        isolation_backend(
                            config,
                            context,
                            environment.clone(),
                            &machine.name,
                            &workspace.name,
                            timeout,
                            Some(backend),
                        )
                        .await?
                        .check,
                    )
                } else {
                    None
                };
                if *stop.borrow() {
                    report.cancelled = true;
                    break;
                }
                let check = if !supported {
                    Check::UnsupportedTransport
                } else if isolation.is_some_and(|c| c != Check::Passed) {
                    isolation.unwrap()
                } else {
                    probe(
                        &boundary,
                        config,
                        context,
                        environment.clone(),
                        machine,
                        &workspace.path,
                        backend,
                        false,
                        timeout,
                    )
                    .await
                };
                report.targets.push(Target {
                    machine: machine.name.clone(),
                    workspace: workspace.name.clone(),
                    backend: backend.clone(),
                    check,
                    isolation,
                });
            }
        }
    }
    report.cancelled |= *stop.borrow();
    report.startup_checks_passed = !report.cancelled
        && report.parent == Check::Passed
        && !report.targets.is_empty()
        && report.targets.iter().all(|t| t.check == Check::Passed);
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
async fn probe(
    boundary: &Isolation,
    config: &Config,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    machine: &Machine,
    workspace: &Path,
    backend: &str,
    parent: bool,
    timeout: Duration,
) -> Check {
    let excluded = vec![
        config.slack.app_token_env.clone(),
        config.slack.user_token_env.clone(),
    ];
    let args = match boundary.readiness(
        (machine.transport == "local").then_some(context.home.as_path()),
        workspace,
        backend,
        &excluded,
        parent,
    ) {
        Ok(args) => args,
        Err(_) => return Check::LaunchConfigurationRefused,
    };
    let control;
    let launch = if machine.transport == "local" {
        LocalTransport {
            machine: machine.clone(),
            home: context.home.clone(),
            excluded_env: excluded,
        }
        .launch(
            args,
            Path::new("/"),
            environment,
            &BTreeMap::new(),
            None,
            false,
        )
    } else {
        control = match tempfile::Builder::new()
            .prefix("fridica-readiness-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
        {
            Ok(control) => control,
            Err(_) => return Check::LaunchConfigurationRefused,
        };
        let mut launch = SshTransport {
            machine: machine.clone(),
            excluded_env: excluded,
            control_directory: control.path().to_owned(),
        }
        .launch(
            args,
            "/",
            environment,
            &BTreeMap::new(),
            LaunchOptions::default(),
        );
        if let Ok(launch) = &mut launch {
            helpers::read_only_ssh_probe(launch);
        }
        launch
    };
    match launch {
        Ok(launch) => helpers::run_readiness(launch, timeout).await,
        Err(_) => Check::LaunchConfigurationRefused,
    }
}
