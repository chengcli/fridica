//! Startup preparation only. Never opens durable state, reads token values,
//! invokes backends, or certifies the later rollout gates.
use super::{isolation_backend, Check};
use crate::{
    config::{registry::Machine, Config, LoadContext},
    exec::isolation::{self as helpers, Isolation},
};
use anyhow::{bail, Result};
use serde::Serialize;
use std::{collections::BTreeMap, ffi::OsString, path::Path, time::Duration};
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
    /// Host paths and the owner's contract.
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
    if crate::config::contract::load(config.owner.contract.as_deref()).is_err() {
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
                    if !*stop.borrow()
                        && crate::github::mcp::target(config, &machine.name, &workspace.policy)
                            .is_some()
                        && !crate::github::mcp::probe(
                            config,
                            &context.home,
                            environment.clone(),
                            machine,
                            workspace,
                            timeout,
                        )
                        .await
                        .unwrap_or(false)
                    {
                        refusals.push(Check::GithubMcpFailed);
                    }
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
    let excluded: Vec<String> = config.secret_env().map(str::to_owned).into();
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
    // The control directory is held until the probe finishes.
    let (launch, _control) = match crate::exec::probe::launch(
        machine,
        &context.home,
        excluded,
        environment,
        args,
        crate::exec::probe::Control::ReadOnly("fridica-readiness-"),
    ) {
        Ok((launch, control)) => (Ok(launch), control),
        Err(error) => (Err(error), None),
    };
    let launch = launch.and_then(|launch| {
        crate::exec::python::activate(
            launch,
            &machine.transport,
            if parent {
                ""
            } else {
                config
                    .python_envs
                    .get(&machine.name)
                    .map(String::as_str)
                    .unwrap_or("")
            },
            Some(&context.home),
        )
    });
    match launch {
        Ok(launch) => helpers::run_readiness(launch, timeout).await,
        Err(_) => vec![Check::LaunchConfigurationRefused],
    }
}
