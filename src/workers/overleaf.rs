//! Native capability follows an admitted task's channel and registered placement.
//! No worker/model input can supply a recipient grant or adapter command.
use super::protocol::WorkerSpec;
use crate::{
    config::{
        overleaf::{Recipient, NATIVE_ENV},
        Config,
    },
    research::Service,
};
use fridica_agent::McpServer;

const JOB_PROJECT: &str = "Thread-confirmed Overleaf project: https://www.overleaf.com/project/";

pub(crate) fn job_project(brief: &str) -> Option<&str> {
    let id = brief.lines().next()?.strip_prefix(JOB_PROJECT)?;
    crate::config::overleaf::valid_id(id).then_some(id)
}

pub(crate) fn project_brief(project: &str, brief: &str) -> String {
    format!("{JOB_PROJECT}{project}\n{brief}")
}

pub(crate) fn recipient(config: &Config, spec: &WorkerSpec) -> Option<Recipient> {
    let channel = spec.native_channel.as_deref()?;
    let scope = config.overleaf.native_scope.as_ref()?;
    let project = spec.thread_project.as_deref()?;
    if !crate::config::overleaf::valid_id(project)
        || scope.project.as_deref().is_some_and(|p| p != project)
    {
        return None;
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
    server
        .args
        .extend(["--thread-project".into(), spec.thread_project.clone()?]);
    server.env_vars.push(NATIVE_ENV.into());
    Some(server)
}
