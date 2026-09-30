//! General non-model diagnostics. Startup readiness deliberately stays separate:
//! these probes invoke backend help/auth/schema commands, but never a session.
use crate::{
    config::{self, registry::Machine, Config, LoadContext},
    exec::{
        isolation::read_only_ssh_probe,
        local::LocalTransport,
        process,
        ssh::{LaunchOptions, SshTransport},
    },
};
use anyhow::{bail, Result};
use serde::Serialize;
use std::{
    collections::BTreeMap, ffi::OsString, os::unix::fs::PermissionsExt, path::Path, time::Duration,
};
use tokio::sync::watch;

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Status {
    Pass,
    Fail,
    Warn,
    Skip,
}
#[derive(Debug, Serialize)]
pub struct Check {
    pub status: Status,
    pub name: String,
    pub detail: &'static str,
}
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub checks: Vec<Check>,
    pub cancelled: bool,
}
impl Report {
    pub fn passed(&self) -> bool {
        !self.cancelled && !self.checks.iter().any(|c| c.status == Status::Fail)
    }
    fn add(&mut self, status: Status, name: impl Into<String>, detail: &'static str) {
        self.checks.push(Check {
            status,
            name: name.into(),
            detail,
        });
    }
    fn test(&mut self, name: impl Into<String>, ok: bool, failure: &'static str) {
        self.add(
            if ok { Status::Pass } else { Status::Fail },
            name,
            if ok { "" } else { failure },
        );
    }
    pub fn text(&self) -> String {
        let mut lines: Vec<_> = self
            .checks
            .iter()
            .map(|c| {
                format!(
                    "{} {}{}{}",
                    match c.status {
                        Status::Pass => "PASS",
                        Status::Fail => "FAIL",
                        Status::Warn => "WARN",
                        Status::Skip => "SKIP",
                    },
                    c.name,
                    if c.detail.is_empty() { "" } else { ": " },
                    c.detail
                )
            })
            .collect();
        let count = |status| self.checks.iter().filter(|c| c.status == status).count();
        lines.push(format!(
            "Checks: {} passed, {} failed, {} warnings, {} skipped.",
            count(Status::Pass),
            count(Status::Fail),
            count(Status::Warn),
            count(Status::Skip)
        ));
        lines.push("Slack authorization and channel membership are verified on start; no model request was made.".into());
        lines.join("\n")
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
    if !(Duration::from_secs(1)..=Duration::from_secs(120)).contains(&timeout) {
        bail!("doctor timeout must be between 1 and 120 seconds");
    }
    let mut report = Report::default();
    report.test(
        "Operating system",
        cfg!(any(target_os = "linux", target_os = "macos")),
        "macOS or Linux is required",
    );
    let config = match config::load(path, context) {
        Ok(config) => config,
        Err(_) => {
            report.add(
                Status::Fail,
                "Configuration",
                "invalid or unreadable configuration; run check-config for details",
            );
            report.add(
                Status::Skip,
                "Everything else",
                "fix the configuration first",
            );
            return Ok(report);
        }
    };
    report.add(Status::Pass, "Configuration", "");
    report.test(
        "Agent contract",
        config::contract::load(config.owner.contract.as_deref()).is_ok(),
        "invalid or unreadable agent contract",
    );
    report.test(
        "Repository list",
        config::repos::load(config.parent.repos.as_deref()).is_ok(),
        "invalid or unreadable repository list",
    );
    for (name, variable, prefix) in [
        ("Slack app token", &config.slack.app_token_env, "xapp-"),
        ("Slack user token", &config.slack.user_token_env, "xoxp-"),
    ] {
        report.test(
            name,
            environment
                .get(std::ffi::OsStr::new(variable))
                .and_then(|v| v.to_str())
                .is_some_and(|v| v.starts_with(prefix)),
            "set the configured token environment variable to the expected token type",
        );
    }
    let scopes = crate::store::diagnostics::slack_scopes(&config.state.path).await;
    match scopes.as_deref() {
        None | Some("unknown") => report.add(Status::Skip,"Slack files:read","scope information is unavailable until recorded by a daemon start"),
        Some(scopes) if scopes.split(',').any(|s| s.trim()=="files:read") => report.add(Status::Pass,"Slack files:read","recorded at the last daemon start; not a live authorization check"),
        Some(_) => report.add(Status::Warn,"Slack files:read","not previously granted; add files:read and reinstall the Slack app for attachment text"),
    }
    let probe = Probe {
        config: &config,
        context,
        environment,
        timeout,
        stop,
    };
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
    for machine in &config.machines.machines {
        if *probe.stop.borrow() {
            break;
        }
        let name = format!("Machine {}", machine.name);
        if machine.transport == "slurm" {
            report.add(
                Status::Skip,
                name,
                "Slurm is registered but unsupported; jobs there fail",
            );
            continue;
        }
        let system = probe.run(machine, "system", "", false, false, "").await;
        report.test(
            name,
            matches!(system.as_str(), "linux" | "darwin"),
            "unsupported system or failed transport; SSH must connect without a prompt",
        );
        if !matches!(system.as_str(), "linux" | "darwin") {
            continue;
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
        }
        let auto = machine
            .workspaces
            .iter()
            .any(|w| w.policy.approvals == "auto");
        for name in &machine.backends {
            backend(&probe, machine, name, false, auto, &mut report).await;
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
        "Worker MCP isolation",
        "run start --check-ready for reviewed inventories, settings and target isolation checks",
    );
    Ok(report)
}

async fn backend(
    probe: &Probe<'_>,
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
    for action in ["capabilities", "auth"] {
        if *probe.stop.borrow() {
            return;
        }
        let result = probe.run(machine, action, backend, parent, auto, "").await;
        let detail = match result.as_str() {
            "passed" => "",
            "missing" => "install the backend on the target PATH",
            "auto_missing" if backend == "claude" => "upgrade Claude for --permission-mode auto",
            "auto_missing" => "upgrade Codex for auto_review",
            "protocol_missing" => {
                "upgrade the backend; required protocol or parent flags are missing"
            }
            "signed_out" => "backend is not signed in; sign in on this target",
            _ => "probe failed, timed out, or exceeded its output limit",
        };
        report.add(
            if result == "passed" {
                Status::Pass
            } else {
                Status::Fail
            },
            format!("{backend} ({role}) {action}"),
            detail,
        );
    }
}

struct Probe<'a> {
    config: &'a Config,
    context: &'a LoadContext,
    environment: BTreeMap<OsString, OsString>,
    timeout: Duration,
    stop: watch::Receiver<bool>,
}
impl Probe<'_> {
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
        let args = vec!["/usr/bin/python3".into(),"-I".into(),"-S".into(),"-c".into(),include_str!("../exec/diagnostics.py").into(),serde_json::json!({
            "action":action,"backend":backend,"parent":parent,"auto":auto,"workspace":workspace,
            "home":(machine.transport=="local").then_some(&self.context.home),"excluded":excluded,"timeout":self.timeout.as_secs_f64()
        }).to_string()];
        let control;
        let launch = if machine.transport == "local" {
            LocalTransport {
                machine: machine.clone(),
                home: self.context.home.clone(),
                excluded_env: excluded,
            }
            .launch(
                args,
                Path::new("/"),
                self.environment.clone(),
                &BTreeMap::new(),
                None,
                false,
            )?
        } else {
            control = tempfile::Builder::new()
                .prefix("fridica-doctor-")
                .permissions(std::fs::Permissions::from_mode(0o700))
                .tempdir()?;
            let mut launch = SshTransport {
                machine: machine.clone(),
                excluded_env: excluded,
                control_directory: control.path().to_owned(),
            }
            .launch(
                args,
                "/",
                self.environment.clone(),
                &BTreeMap::new(),
                LaunchOptions::default(),
            )?;
            read_only_ssh_probe(&mut launch);
            launch
        };
        let result =
            process::run_with_open_stdin(launch, self.timeout + Duration::from_secs(3), 4096)
                .await?;
        if result.returncode != 0 {
            bail!("diagnostic transport failed");
        }
        let text = result.text();
        let value = text
            .strip_prefix("fridica-doctor:")
            .and_then(|s| s.strip_suffix('\n'))
            .unwrap_or("failed");
        Ok(match value {
            "passed" | "linux" | "darwin" | "missing" | "auto_missing" | "protocol_missing"
            | "signed_out" => value,
            _ => "failed",
        }
        .into())
    }
}
