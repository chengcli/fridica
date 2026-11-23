//! General non-model diagnostics. Startup readiness deliberately stays separate:
//! these probes invoke backend help/auth/schema commands, but never a session.
use crate::{
    config::{self, registry::Machine, Config, LoadContext},
    exec::process,
};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::Path, sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Pass,
    Fail,
    Warn,
    Skip,
}
#[derive(Clone, Debug, Serialize)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: String,
}
/// What the loaded configuration resolves to, for the deployment record.
/// Names and counts only: no paths, hosts or token values.
#[derive(Clone, Debug, Serialize)]
pub struct Configuration {
    pub fingerprint: String,
    pub machines: Vec<String>,
    pub default_machine: String,
    pub attention: crate::core::config::Attention,
}
/// What a watcher of a running doctor sees, in the order it happens.
#[derive(Clone, Debug)]
pub enum Progress {
    Configuration(Configuration),
    Check(Check),
}
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration: Option<Configuration>,
    /// Each check is also sent here the moment it is known.
    #[serde(skip)]
    progress: Option<mpsc::UnboundedSender<Progress>>,
}
impl Report {
    pub fn passed(&self) -> bool {
        !self.cancelled && !self.checks.iter().any(|c| c.status == Status::Fail)
    }
    /// Stop streaming: a watcher's receiver then ends once every machine's
    /// report is gone. Called by the runner at the end, and safe to repeat.
    pub fn detach(&mut self) {
        self.progress = None;
    }
    pub(super) fn add(
        &mut self,
        status: Status,
        name: impl Into<String>,
        detail: impl Into<String>,
    ) {
        let check = Check {
            status,
            name: name.into(),
            detail: detail.into(),
        };
        if let Some(progress) = &self.progress {
            let _ = progress.send(Progress::Check(check.clone()));
        }
        self.checks.push(check);
    }
    fn test(&mut self, name: impl Into<String>, ok: bool, failure: &'static str) {
        self.add(
            if ok { Status::Pass } else { Status::Fail },
            name,
            if ok { "" } else { failure },
        );
    }
    pub fn text(&self) -> String {
        let mut lines = vec![];
        if let Some(c) = &self.configuration {
            lines.push(c.text());
        }
        lines.extend(self.checks.iter().map(Check::text));
        lines.push(self.summary());
        lines.join("\n")
    }
    /// The closing lines: counts, and what doctor does not check.
    pub fn summary(&self) -> String {
        let count = |status| self.checks.iter().filter(|c| c.status == status).count();
        format!(
            "Checks: {} passed, {} failed, {} warnings, {} skipped.\nSlack owner identity and channel membership are verified on start; no model request was made.",
            count(Status::Pass),
            count(Status::Fail),
            count(Status::Warn),
            count(Status::Skip)
        )
    }
}
impl Configuration {
    pub fn text(&self) -> String {
        format!(
            "Configuration fingerprint: {}\nMachines: {} (default: {})",
            self.fingerprint,
            self.machines.join(", "),
            self.default_machine
        )
    }
}
impl Check {
    pub fn text(&self) -> String {
        format!(
            "{} {}{}{}",
            match self.status {
                Status::Pass => "PASS",
                Status::Fail => "FAIL",
                Status::Warn => "WARN",
                Status::Skip => "SKIP",
            },
            self.name,
            if self.detail.is_empty() { "" } else { ": " },
            self.detail
        )
    }
}

/// No state creation/migration, Slack calls or model requests. Stop is observed
/// between bounded probes; the current probe completes and reaps before return.
pub async fn run(
    path: &Path,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    stop: watch::Receiver<bool>,
) -> Result<Report> {
    run_with(path, context, environment, timeout, stop, None, false).await
}
/// Like `run`, streaming each result to `progress` as soon as it is known.
pub async fn run_with(
    path: &Path,
    context: &LoadContext,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    stop: watch::Receiver<bool>,
    progress: Option<mpsc::UnboundedSender<Progress>>,
    live_slack: bool,
) -> Result<Report> {
    if !(Duration::from_secs(1)..=Duration::from_secs(120)).contains(&timeout) {
        bail!("doctor timeout must be between 1 and 120 seconds");
    }
    let mut report = Report {
        progress,
        ..Report::default()
    };
    report.test(
        "Operating system",
        cfg!(any(target_os = "linux", target_os = "macos")),
        "macOS or Linux is required",
    );
    let config = match config::load(path, context) {
        Ok(config) => config,
        Err(error) => {
            let absent = std::fs::symlink_metadata(path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound);
            if absent {
                report.add(
                    Status::Skip,
                    "Configuration",
                    "run fridica init to create the starter configuration",
                );
            } else {
                let reason = format!("{error:#}");
                report.add(
                    Status::Fail,
                    "Configuration",
                    format!(
                        "invalid or unreadable configuration: {}",
                        reason.lines().next().unwrap_or("")
                    ),
                );
            }
            let source = if absent {
                Ok(crate::secrets::starter_config())
            } else {
                config::editor::read(path)
            };
            if let Some((slack, github)) = source
                .ok()
                .and_then(|s| config::loader::credential_settings(&s).ok())
            {
                super::credential_checks::check(
                    &mut report,
                    &slack,
                    &github,
                    &environment,
                    timeout,
                    live_slack,
                )
                .await;
            }
            report.add(
                Status::Skip,
                "Everything else",
                "complete configuration first",
            );
            report.detach();
            return Ok(report);
        }
    };
    report.add(Status::Pass, "Configuration", "");
    let configuration = Configuration {
        fingerprint: config.fingerprint.clone(),
        machines: config.machines.names(),
        default_machine: config.parent.default_machine.clone(),
        attention: config.attention.clone(),
    };
    if let Some(progress) = &report.progress {
        let _ = progress.send(Progress::Configuration(configuration.clone()));
    }
    report.configuration = Some(configuration);
    report.test(
        "Agent contract",
        config::contract::load(config.owner.contract.as_deref()).is_ok(),
        "invalid or unreadable agent contract",
    );
    super::credential_checks::check(
        &mut report,
        &config.slack,
        &config.github,
        &environment,
        timeout,
        live_slack,
    )
    .await;
    let scopes = crate::store::diagnostics::slack_scopes(&config.state.path).await;
    match scopes.as_deref() {
        None | Some("unknown") => report.add(Status::Skip,"Slack files:read","scope information is unavailable until recorded by a daemon start"),
        Some(scopes) if scopes.split(',').any(|s| s.trim()=="files:read") => report.add(Status::Pass,"Slack files:read","recorded at the last daemon start; not a live authorization check"),
        Some(_) => report.add(Status::Warn,"Slack files:read","not previously granted; add files:read and reinstall the Slack app for attachment text"),
    }
    // Under /tmp: a Unix socket path must stay short, and the per-user
    // temporary directory on macOS is already most of the limit.
    let control = tempfile::Builder::new()
        .prefix("fridica-doctor-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in("/tmp")?;
    let probe = Arc::new(Probe {
        config: Arc::new(config),
        context: context.clone(),
        environment,
        timeout,
        stop,
        control: control.path().to_owned(),
    });
    let config = probe.config.clone();
    let mut parent = config.machines.machines[0].clone();
    parent.transport = "local".into();
    parent.resources = Default::default();
    backend(
        &probe,
        &parent,
        &config.parent.backend,
        true,
        false,
        &mut report,
    )
    .await;
    // Machines are independent, so they are probed at the same time: a slow or
    // unreachable host then costs its own probes' time, not everyone's.
    if !*probe.stop.borrow() {
        let tasks: Vec<_> = config
            .machines
            .machines
            .iter()
            .map(|m| tokio::spawn(machine(probe.clone(), m.clone(), report.progress.clone())))
            .collect();
        // Results keep the configuration's order whatever finishes first.
        for task in tasks {
            let part = task.await.context("machine probes stopped unexpectedly")?;
            report.checks.extend(part.checks);
        }
    }
    report.cancelled = *probe.stop.borrow();
    if report.cancelled {
        report.add(
            Status::Fail,
            "Diagnostics cancelled",
            "remaining probes were not run",
        );
    }
    report.add(
        Status::Skip,
        "Worker isolation",
        "run start --check-ready for target isolation checks",
    );
    // After the last check, so a watcher is shown every line the counts include.
    report.detach();
    Ok(report)
}

/// Every check for one machine, in a report of its own.
async fn machine(
    probe: Arc<Probe>,
    machine: Machine,
    progress: Option<mpsc::UnboundedSender<Progress>>,
) -> Report {
    let (probe, machine) = (&*probe, &machine);
    let mut report = Report {
        progress,
        ..Report::default()
    };
    {
        let name = format!("Machine {}", machine.name);
        if machine.transport == "slurm" {
            report.add(
                Status::Skip,
                name,
                "Slurm is registered but unsupported; jobs there fail",
            );
            return report;
        }
        let system = probe.run(machine, "system", "", false, false, "").await;
        report.test(
            name,
            matches!(system.as_str(), "linux" | "darwin"),
            if system == "python_environment_unavailable" {
                "configured Python environment is missing or failed to activate"
            } else {
                "unsupported system or failed transport; SSH must connect without a prompt"
            },
        );
        if !matches!(system.as_str(), "linux" | "darwin") {
            return report;
        }
        for workspace in &machine.workspaces {
            if *probe.stop.borrow() {
                break;
            }
            let result = probe
                .run(
                    machine,
                    "workspace",
                    "",
                    false,
                    false,
                    &workspace.path.to_string_lossy(),
                )
                .await;
            report.test(
                format!("Machine {} workspace {}", machine.name, workspace.name),
                result == "passed",
                "workspace is not an accessible directory on this target",
            );
            if crate::github::mcp::target(&probe.config, &machine.name, &workspace.policy).is_some()
                && !*probe.stop.borrow()
            {
                let passed = crate::github::mcp::probe(
                    &probe.config,
                    &probe.context.home,
                    probe.environment.clone(),
                    machine,
                    workspace,
                    probe.timeout,
                )
                .await
                .unwrap_or(false);
                report.test(format!("GitHub MCP {}:{}", machine.name, workspace.name), passed,
                    "GitHub MCP executable, authentication, initialization or required tools unavailable");
            }
            for service in crate::research::SERVICES {
                if *probe.stop.borrow() || !service.enabled(&probe.config) {
                    continue;
                }
                use crate::research::Service;
                let (label, success) = match service {
                    Service::Scix => ("NASA ADS (SciX)", "ADS accepted the worker token"),
                    Service::Overleaf => (
                        "Overleaf",
                        "Overleaf accepted the worker token for the configured project",
                    ),
                    Service::Alphaxiv => {
                        ("alphaXiv", "authenticated and discovered research tools")
                    }
                };
                let name = format!("{label} MCP {}:{}", machine.name, workspace.name);
                let Some(server) = service.server_at(
                    &probe.config,
                    &machine.name,
                    &workspace.policy,
                    Some(&workspace.path),
                ) else {
                    report.add(Status::Skip, name, "workspace policy excludes this server");
                    continue;
                };
                let passed = match crate::workers::mcp_probe::launch(
                    &probe.config,
                    &probe.context.home,
                    probe.environment.clone(),
                    machine,
                    workspace,
                    server,
                ) {
                    Ok((launch, _control)) => {
                        let marker = format!("fridica-{}-mcp:ready\n", service.name());
                        match crate::exec::process::run_with_open_stdin(launch, probe.timeout, 4096)
                            .await
                        {
                            Ok(r) if r.returncode == 0 && matches!(service, Service::Overleaf) => {
                                if r.stdout == marker.as_bytes() {
                                    report.add(Status::Warn, name, "bundled tools are ready; set OVERLEAF_PROJECT_ID to verify the token against a project");
                                    continue;
                                }
                                r.stdout == b"fridica-overleaf-mcp:authenticated\n"
                            }
                            Ok(r) => r.returncode == 0 && r.stdout == marker.as_bytes(),
                            Err(_) => false,
                        }
                    }
                    Err(_) => false,
                };
                if passed {
                    report.add(Status::Pass, name, success);
                } else {
                    report.add(Status::Fail, name, format!("check the worker binary, credentials and service configuration with fridica {}-mcp --probe", service.name()));
                }
            }
        }
        let auto = machine
            .workspaces
            .iter()
            .any(|w| w.policy.approvals == "auto");
        for name in &machine.backends {
            backend(probe, machine, name, false, auto, &mut report).await;
        }
        if system == "linux" && !*probe.stop.borrow() {
            let backend = if machine.backends.iter().any(|b| b == "claude") {
                "claude"
            } else {
                "codex"
            };
            let result = probe
                .run(machine, "sandbox", backend, false, false, "")
                .await;
            report.test(
                format!("Machine {} sandbox", machine.name),
                result == "passed",
                "install bubblewrap (and socat for Claude) and allow bubblewrap user namespaces",
            );
        }
    }
    report
}

/// One line per backend and role: its shortcomings are independent facets
/// (protocol, automatic approvals, sign-in), all reported at once.
async fn backend(
    probe: &Probe,
    machine: &Machine,
    backend: &str,
    parent: bool,
    auto: bool,
    report: &mut Report,
) {
    let role = if parent {
        "parent locally".to_owned()
    } else {
        format!("worker on {}", machine.name)
    };
    if *probe.stop.borrow() {
        return;
    }
    let result = probe
        .run(machine, "backend", backend, parent, auto, "")
        .await;
    let detail: Vec<&str> = result
        .split(',')
        .map(|code| match code {
            "passed" => "",
            "missing" => "install the backend on the target PATH",
            "auto_missing" if backend == "claude" => "upgrade Claude for --permission-mode auto",
            "auto_missing" => "upgrade Codex for auto_review",
            "protocol_missing" => {
                "upgrade the backend; required protocol or parent flags are missing"
            }
            "signed_out" => "backend is not signed in; sign in on this target",
            _ => "probe failed, timed out, or exceeded its output limit",
        })
        .collect();
    report.add(
        if result == "passed" {
            Status::Pass
        } else {
            Status::Fail
        },
        format!("{backend} ({role})"),
        detail.join("; "),
    );
}

struct Probe {
    config: Arc<Config>,
    context: LoadContext,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    stop: watch::Receiver<bool>,
    /// One private SSH control directory for the whole run, so a machine's
    /// probes share a connection instead of each paying a full handshake.
    control: std::path::PathBuf,
}
impl Probe {
    #[allow(clippy::too_many_arguments)]
    async fn run(
        &self,
        machine: &Machine,
        action: &str,
        backend: &str,
        parent: bool,
        auto: bool,
        workspace: &str,
    ) -> String {
        self.try_run(machine, action, backend, parent, auto, workspace)
            .await
            .unwrap_or_else(|_| "failed".into())
    }
    #[allow(clippy::too_many_arguments)]
    async fn try_run(
        &self,
        machine: &Machine,
        action: &str,
        backend: &str,
        parent: bool,
        auto: bool,
        workspace: &str,
    ) -> Result<String> {
        let excluded: Vec<_> = self
            .config
            .secret_env()
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = crate::exec::helper::argv(vec![include_str!("../exec/diagnostics.py").into(),serde_json::json!({
            "action":action,"backend":backend,"parent":parent,"auto":auto,"workspace":workspace,
            "home":(machine.transport=="local").then_some(&self.context.home),"excluded":excluded,"timeout":self.timeout.as_secs_f64()
        }).to_string()]);
        let (launch, _) = crate::exec::probe::launch(
            machine,
            &self.context.home,
            excluded,
            self.environment.clone(),
            args,
            crate::exec::probe::Control::Shared(&self.control),
        )?;
        let launch = crate::exec::python::activate(
            launch,
            &machine.transport,
            if parent {
                ""
            } else {
                self.config
                    .python_envs
                    .get(&machine.name)
                    .map(String::as_str)
                    .unwrap_or("")
            },
            Some(&self.context.home),
        )?;
        let result =
            process::run_with_open_stdin(launch, self.timeout + Duration::from_secs(3), 4096)
                .await?;
        if crate::exec::python::refused(result.returncode, &result.stderr) {
            return Ok("python_environment_unavailable".into());
        }
        if result.returncode != 0 {
            bail!("diagnostic transport failed");
        }
        let text = result.text();
        let value = text
            .strip_prefix("fridica-doctor:")
            .and_then(|s| s.strip_suffix('\n'))
            .unwrap_or("failed");
        // A backend probe lists its shortcomings as a comma-separated set.
        let known = |code: &str| {
            matches!(
                code,
                "passed"
                    | "linux"
                    | "darwin"
                    | "missing"
                    | "auto_missing"
                    | "protocol_missing"
                    | "signed_out"
            )
        };
        Ok(if !value.is_empty() && value.split(',').all(known) {
            value
        } else {
            "failed"
        }
        .into())
    }
}
