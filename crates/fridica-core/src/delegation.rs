//! Pure validation/placement before committing an actor turn. IDs are allocated
//! only after the entire response passes validation, including repair attempts.
use crate::{
    config::{
        registry::{valid_fetch_ref, Registry},
        Limits,
    },
    parent::{Decision, ParentRequest, WorkerOperation},
    placement::{self, Selector},
    time::Identifiers,
    worker::{Job, WorkerRecord},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// What a turn may delegate to, from the host's configuration.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    /// Whether the thread's channel may delegate at all.
    pub allowed: bool,
    pub limits: &'a Limits,
    pub machines: &'a Registry,
}
#[derive(Default)]
pub struct Work {
    pub workers: Vec<WorkerRecord>,
    pub jobs: Vec<Job>,
    pub context: Value,
}

/// Validate a decision's context change, worker controls and delegations, and
/// place new workers. With `ids`, also allocate worker and job IDs; without,
/// only validate (as before a repair round is accepted).
pub fn prepare(
    decision: &Decision,
    request: &ParentRequest,
    scope: Option<Scope<'_>>,
    ids: Option<&dyn Identifiers>,
) -> Result<Work> {
    let mut work = Work {
        context: context(decision, request, scope.map(|s| s.machines))?,
        ..Work::default()
    };
    let existing: Vec<WorkerRecord> =
        serde_json::from_value(request.session["work"]["workers"].clone())?;
    let mut controlled = HashSet::new();
    for control in &decision.worker_control {
        if !existing.iter().any(|w| {
            w.id == control.worker_id
                && w.session_id == request.session["id"].as_str().unwrap_or("")
        }) {
            bail!("controlled worker is not in this thread");
        }
        if !controlled.insert(&control.worker_id) {
            bail!("only one control per worker per turn");
        }
        if control.op == WorkerOperation::Stop
            && decision
                .delegations
                .iter()
                .any(|d| d.worker_id == control.worker_id)
        {
            bail!("cannot delegate to a worker being stopped");
        }
    }
    if decision.delegations.is_empty() {
        return Ok(work);
    }
    let scope = scope.context("delegation is not configured")?;
    request.session["channel"]
        .as_str()
        .context("missing channel")?;
    if !scope.allowed {
        bail!("delegation is disabled in this channel");
    }
    if decision.delegations.len() > scope.limits.max_delegations_per_turn {
        bail!("too many delegations in one turn");
    }
    let busy: BTreeMap<String, usize> =
        serde_json::from_value(request.session["work"]["busy"].clone())?;
    // Recorded probe readings (absent when probing is off or unavailable).
    let load: BTreeMap<String, placement::probe::Assessment> =
        match &request.session["work"]["load"] {
            Value::Null => BTreeMap::new(),
            value => serde_json::from_value(value.clone())?,
        };
    let mut live = existing
        .iter()
        .filter(|w| !w.ephemeral && w.status != "stopped")
        .count();
    let session = request.session["id"].as_str().context("missing session")?;
    let sticky = &request.session["context"];
    // One snapshot of this turn for every forked job (a worker fork keeps it
    // as its fallback); only when IDs are allocated, since a repair round may
    // still change the decision.
    let snapshot = ids
        .filter(|_| {
            decision
                .delegations
                .iter()
                .any(|d| d.context != crate::fork::ContextMode::Fresh)
        })
        .map(|_| crate::fork::snapshot(request, decision, scope.limits.worker_context_chars));
    for d in &decision.delegations {
        if d.brief.trim().is_empty() || d.brief.chars().count() > 40000 {
            bail!("delegation needs a brief of at most 40000 characters");
        }
        let deliverable = if d.deliverable.is_empty() {
            "report"
        } else {
            &d.deliverable
        };
        if !matches!(deliverable, "report" | "markdown" | "figures_pdf") {
            bail!("invalid deliverable");
        }
        let mut worker = if d.worker_id.is_empty() {
            if !d.ephemeral {
                live += 1;
                if live > scope.limits.max_workers_per_thread {
                    bail!("too many persistent workers in this thread");
                }
            }
            let placement = placement::resolve(
                scope.machines,
                &Selector {
                    machine: d.machine.clone(),
                    // The schema allows "" (no tag) alongside known tags.
                    tags: d.tags.iter().filter(|t| !t.is_empty()).cloned().collect(),
                    workspace: d.workspace.clone(),
                    backend: d.backend.clone(),
                },
                sticky["machine"].as_str().unwrap_or(""),
                sticky["workspace"].as_str().unwrap_or(""),
                &busy,
                &load,
            )?;
            let role = if d.role.is_empty() {
                "general"
            } else {
                &d.role
            };
            if !matches!(role, "general" | "implementer" | "reviewer" | "tester") {
                bail!("invalid worker role");
            }
            serde_json::from_value::<WorkerRecord>(json!({"id":"", "session_id":session,
                "machine":placement.machine.name,"workspace":placement.workspace.name,"backend":placement.backend,
                "role":role,"ephemeral":d.ephemeral}))?
        } else {
            existing
                .iter()
                .find(|w| w.id == d.worker_id && w.status != "stopped")
                .context("worker_id is not a live worker of this thread (see session.work.workers); leave worker_id empty to start a new worker")?
                .clone()
        };
        // A worker fork copies a live backend session, which only the same
        // backend on the same machine can open; anything else repairs to a
        // thread fork.
        let fork_from_worker = if d.context == crate::fork::ContextMode::ForkWorker {
            if d.fork_worker_id.is_empty() {
                bail!("context fork_worker needs fork_worker_id, a live worker of this thread with a backend session (see session.work.workers); use context: fork to fork the thread instead");
            }
            if !d.worker_id.is_empty() {
                bail!("context fork_worker starts a new worker from a copy of fork_worker_id's session; leave worker_id empty, or delegate to worker_id with context: fork");
            }
            let source = existing
                .iter()
                .find(|w| {
                    w.id == d.fork_worker_id && w.session_id == session && w.status != "stopped"
                })
                .context("fork_worker_id is not a live worker of this thread (see session.work.workers); use context: fork instead")?;
            if source.backend_session_id.is_empty() {
                bail!(
                    "fork_worker_id has no backend session to fork yet; use context: fork instead"
                );
            }
            if source.backend != worker.backend || source.machine != worker.machine {
                bail!("a fork_worker delegation must place the new worker on the same machine and backend as fork_worker_id (set machine, workspace and backend to match), or use context: fork instead");
            }
            source.id.clone()
        } else {
            if !d.fork_worker_id.is_empty() {
                bail!("fork_worker_id needs context: fork_worker");
            }
            String::new()
        };
        let machine = scope
            .machines
            .get(&worker.machine)
            .context("worker machine no longer configured")?;
        let workspace = machine
            .workspace(&worker.workspace)
            .context("worker workspace no longer configured")?;
        if !machine.backends.contains(&worker.backend) {
            bail!("worker backend no longer configured");
        }
        // Scoped fetch is only for owner-granted repositories; errors say how to
        // repair, because the parent sees them in its one repair round.
        let fetch_repo = if d.fetch_repo.is_empty() {
            if !d.fetch_ref.is_empty() {
                bail!("fetch_ref needs fetch_repo; leave both empty so the worker clones the repository itself");
            }
            String::new()
        } else {
            let granted = workspace
                .policy
                .fetch_grant(&d.fetch_repo)
                .context("fetch_repo is not granted in this workspace's fetch_repos; leave fetch_repo and fetch_ref empty so the worker clones or reuses a checkout itself")?
                .to_string();
            if !valid_fetch_ref(&d.fetch_ref) {
                bail!("invalid fetch_ref; use refs/heads/BRANCH, refs/pull/N/head, a commit SHA or HEAD");
            }
            granted
        };
        // Only files attached in this thread, each once, and few enough to
        // place before the job starts.
        let files: Vec<String> = d.files.iter().filter(|f| !f.is_empty()).cloned().collect();
        if files.len() > 3 {
            bail!("at most 3 files per delegation");
        }
        for (i, file) in files.iter().enumerate() {
            if files[..i].contains(file) {
                bail!("a file is listed twice");
            }
            if !request.session["files"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|f| f["id"] == file.as_str())
            {
                bail!("files must name attachments of this thread (see session.files)");
            }
        }
        if let Some(ids) = ids {
            if worker.id.is_empty() {
                worker.id = ids.next("worker");
                work.workers.push(worker.clone());
                if !worker.ephemeral {
                    work.context["machine"] = json!(worker.machine);
                    work.context["workspace"] = json!(worker.workspace);
                }
            }
            let forked = (d.context != crate::fork::ContextMode::Fresh)
                .then(|| snapshot.clone())
                .flatten();
            work.jobs.push(serde_json::from_value(json!({"id":ids.next("job"),"worker_id":worker.id,
                "session_id":session,"brief":d.brief.trim(),"join_group":request.inbox_id.to_string(),
                "inbox_id":request.inbox_id,"deliverable":deliverable,"fetch_repo":fetch_repo,"fetch_ref":d.fetch_ref,"files":files,
                "context":d.context,"snapshot":forked,"fork_from_worker":fork_from_worker}))?);
        }
    }
    Ok(work)
}
/// The thread context patch a decision asks for (machine, workspace, repo,
/// branch), checked against the registry.
pub fn context(
    decision: &Decision,
    request: &ParentRequest,
    machines: Option<&Registry>,
) -> Result<Value> {
    let context = &decision.context;
    if !context.machine.is_empty() || !context.workspace.is_empty() {
        let registry = machines.context("context placement is not configured")?;
        let target = if context.machine.is_empty() {
            request.session["context"]["machine"]
                .as_str()
                .filter(|s| !s.is_empty())
                .unwrap_or(&registry.default)
        } else {
            &context.machine
        };
        let machine = registry.get(target).context("unknown context machine")?;
        if !context.workspace.is_empty() && machine.workspace(&context.workspace).is_none() {
            bail!("context workspace is not on the selected machine");
        }
        // A machine-only update must not retain an invalid workspace on the new
        // machine. The parent can repair the pair explicitly.
        let workspace = request.session["context"]["workspace"]
            .as_str()
            .unwrap_or("");
        if context.workspace.is_empty()
            && !workspace.is_empty()
            && machine.workspace(workspace).is_none()
        {
            bail!("context machine change needs a valid workspace");
        }
    }
    let mut patch = serde_json::to_value(context)?;
    patch
        .as_object_mut()
        .unwrap()
        .retain(|_, v| v.as_str().is_some_and(|s| !s.is_empty()));
    Ok(patch)
}
