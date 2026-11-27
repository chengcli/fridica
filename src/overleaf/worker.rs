//! Native capability follows an admitted task's channel and registered placement.
//! No worker/model input can supply a recipient grant or adapter command.
use crate::workers::protocol::WorkerSpec;
use crate::{
    config::{
        overleaf::{Recipient, NATIVE_ENV},
        Config,
    },
    research::Service,
};
use fridica_agent::McpServer;

const JOB_PROJECT: &str = "Thread-confirmed Overleaf project ID: ";
const LEGACY_JOB_PROJECT: &str =
    "Thread-confirmed Overleaf project: https://www.overleaf.com/project/";

pub(crate) fn job_project(brief: &str) -> Option<&str> {
    let first = brief.lines().next()?;
    let id = first
        .strip_prefix(JOB_PROJECT)
        .or_else(|| first.strip_prefix(LEGACY_JOB_PROJECT))?;
    crate::config::overleaf::valid_id(id).then_some(id)
}

pub(crate) fn project_brief(project: &str, brief: &str) -> String {
    format!("{JOB_PROJECT}{project}\n{brief}")
}

pub(crate) fn recipient(config: &Config, spec: &WorkerSpec) -> Option<Recipient> {
    let channel = spec.native_channel.as_deref()?;
    let scope = config.overleaf.native_scope.as_ref()?;
    if let Some(project) = spec.thread_project.as_deref() {
        if !crate::config::overleaf::valid_id(project)
            || scope.project.as_deref().is_some_and(|p| p != project)
        {
            return None;
        }
    }
    let machine = config.machines.get(&spec.machine.name)?;
    let workspace = machine.workspace(&spec.workspace.name)?;
    if !config.slack.channels.iter().any(|c| c == channel)
        || scope.channel.as_deref().is_some_and(|c| c != channel)
        || machine.for_slot(spec.slot) != spec.machine
        || workspace.for_slot(spec.slot) != spec.workspace
        || !machine.backends.contains(&spec.backend)
        || !Service::Overleaf.available(config, &machine.name, &workspace.policy)
        || spec.excluded_env.iter().any(|v| v == NATIVE_ENV)
    {
        return None;
    }
    let actual = Recipient {
        worker_id: spec.worker_id.clone(),
        machine: machine.name.clone(),
        workspace: workspace.name.clone(),
        expires_at: None,
    };
    actual.validate().ok()?;
    match &config.overleaf.native_recipient {
        Some(limit) if limit.permits(&actual.worker_id, &actual.machine, &actual.workspace) => {
            Some(limit.clone())
        }
        Some(_) => None,
        None => Some(actual),
    }
}

pub(crate) fn server(config: &Config, spec: &WorkerSpec) -> Option<McpServer> {
    recipient(config, spec)?;
    let mut server = Service::Overleaf.server_at(
        config,
        &spec.machine.name,
        &spec.workspace.policy,
        Some(&spec.workspace.path),
    )?;
    let channel = spec.native_channel.as_ref()?;
    server.args = vec![
        "overleaf-mcp".into(),
        "--workspace-write".into(),
        spec.workspace.path.to_string_lossy().into_owned(),
        "--native-channel".into(),
        channel.clone(),
        "--native-worker".into(),
        spec.worker_id.clone(),
    ];
    // Native tools need only the scoped owner session; never pass Git/ADS credentials.
    server.env_vars.clear();
    server.args.extend([
        "--thread-project".into(),
        spec.thread_project
            .clone()
            .unwrap_or_else(|| "unbound".into()),
    ]);
    server.env_vars.push(NATIVE_ENV.into());
    Some(server)
}

/// Bind the stored owner grant to the admitted task and include its receipt roots.
pub(crate) fn bind_grant(config: &Config, spec: &WorkerSpec, raw: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let recipient = recipient(config, spec).context("native recipient unavailable")?;
    let mut scope = config
        .overleaf
        .native_scope
        .clone()
        .context("native scope missing")?;
    if let Some(project) = &spec.thread_project {
        scope.project = Some(project.clone());
    }
    crate::config::overleaf::bound_session(
        raw,
        &recipient,
        config.overleaf.native_recipient.as_ref(),
        &scope,
        spec.native_channel
            .as_deref()
            .context("missing native channel")?,
        config.overleaf.origin(),
        config
            .machines
            .get(&spec.machine.name)
            .context("missing native machine")?
            .workspaces
            .iter()
            .map(|w| w.path.clone()),
    )
}
