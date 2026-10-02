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

/// Refusals are independent facets, every one of them reported, so a single
/// run shows all there is to fix; an empty list means ready.
#[derive(Debug, Serialize)]
pub struct Target {
    pub machine: String,
    pub workspace: String,
    pub backend: String,
    pub refusals: Vec<Check>,
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub build_version: &'static str,
    pub config_fingerprint: String,
    /// Host paths and the owner's contract and repository files.
    pub host: Vec<Check>,
    pub parent_backend: String,
    pub parent: Vec<Check>,
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
        host: vec![],
        parent_backend: config.parent.backend.clone(),
        parent: vec![Check::ProbeFailed],
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
        report.host.push(Check::HostPathsRefused);
    }
    if crate::config::contract::load(config.owner.contract.as_deref()).is_err()
        || crate::config::repos::load(config.parent.repos.as_deref()).is_err()
    {
        report.host.push(Check::OwnerInputsRefused);
    }
    if !report.host.is_empty() {
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
                // Every facet is judged, so one failure does not hide another.
                let mut refusals = vec![];
                if !supported {
                    refusals.push(Check::UnsupportedTransport);
                } else {
                    if workspace.policy.gpu_confine == Some(true) {
                        let confinement = isolation_backend(
                            config,
                            context,
                            environment.clone(),
                            &machine.name,
                            &workspace.name,
                            timeout,
                            Some(backend),
                        )
                        .await?
                        .check;
                        if confinement != Check::Passed {
                            refusals.push(confinement);
                        }
                    }
                    if *stop.borrow() {
                        report.cancelled = true;
                        break;
                    }
                    refusals.extend(
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
                        .await,
                    );
                    refusals.sort();
                    refusals.dedup();
                }
                report.targets.push(Target {
                    machine: machine.name.clone(),
                    workspace: workspace.name.clone(),
                    backend: backend.clone(),
                    refusals,
                });
            }
        }
    }
    report.cancelled |= *stop.borrow();
    report.startup_checks_passed = !report.cancelled
        && report.host.is_empty()
        && report.parent.is_empty()
        && !report.targets.is_empty()
        && report.targets.iter().all(|t| t.refusals.is_empty());
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
) -> Vec<Check> {
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
        Err(_) => return vec![Check::LaunchConfigurationRefused],
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
            Err(_) => return vec![Check::LaunchConfigurationRefused],
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
        Err(_) => vec![Check::LaunchConfigurationRefused],
    }
}
