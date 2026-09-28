//! Pure validation/placement before committing an actor turn. IDs are allocated
//! only after the entire response passes validation, including repair attempts.
use crate::{
    config::{registry::valid_fetch_ref, Config},
    core::{
        parent::{Decision, ParentRequest},
        time::Identifiers,
        worker::{Job, WorkerRecord},
    },
    machines::{self, Selector},
};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use unicode_casefold::UnicodeCaseFold;

#[derive(Default)]
pub(super) struct Work {
    pub workers: Vec<WorkerRecord>,
    pub jobs: Vec<Job>,
    pub context: Value,
}

pub(super) fn prepare(
    decision: &Decision,
    request: &ParentRequest,
    config: Option<&Config>,
    ids: Option<&dyn Identifiers>,
) -> Result<Work> {
    let mut work = Work {
        context: super::effects::context(decision, request, config)?,
        ..Work::default()
    };
    if decision.delegations.is_empty() {
        return Ok(work);
    }
    let config = config.context("delegation is not configured")?;
    let channel = request.session["channel"]
        .as_str()
        .context("missing channel")?;
    if !config.slack.channels.iter().any(|c| c == channel) || !config.slack.may_delegate(channel) {
        bail!("delegation is disabled in this channel");
    }
    if decision.delegations.len() > config.limits.max_delegations_per_turn {
        bail!("too many delegations in one turn");
    }
    let existing: Vec<WorkerRecord> =
        serde_json::from_value(request.session["work"]["workers"].clone())?;
    let busy: BTreeMap<String, usize> =
        serde_json::from_value(request.session["work"]["busy"].clone())?;
    let mut live = existing
        .iter()
        .filter(|w| !w.ephemeral && w.status != "stopped")
        .count();
    let session = request.session["id"].as_str().context("missing session")?;
    let sticky = &request.session["context"];
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
                if live > config.limits.max_workers_per_thread {
                    bail!("too many persistent workers in this thread");
                }
            }
            let placement = machines::resolve(
                &config.machines,
                &Selector {
                    machine: d.machine.clone(),
                    tags: d.tags.clone(),
                    workspace: d.workspace.clone(),
                    backend: d.backend.clone(),
                },
                sticky["machine"].as_str().unwrap_or(""),
                sticky["workspace"].as_str().unwrap_or(""),
                &busy,
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
                .context("worker is unavailable in this thread")?
                .clone()
        };
        let machine = config
            .machines
            .get(&worker.machine)
            .context("worker machine no longer configured")?;
        let workspace = machine
            .workspace(&worker.workspace)
            .context("worker workspace no longer configured")?;
        if !machine.backends.contains(&worker.backend) {
            bail!("worker backend no longer configured");
        }
        let fetch_repo = if d.fetch_repo.is_empty() {
            if !d.fetch_ref.is_empty() {
                bail!("fetch_ref needs fetch_repo");
            }
            String::new()
        } else {
            if !valid_fetch_ref(&d.fetch_ref) {
                bail!("invalid fetch ref");
            }
            workspace
                .policy
                .fetch_repos
                .iter()
                .find(|r| r.as_str().case_fold().eq(d.fetch_repo.as_str().case_fold()))
                .context("fetch repository is not granted")?
                .clone()
        };
        if let Some(ids) = ids {
            if worker.id.is_empty() {
                worker.id = ids.next("worker");
                work.workers.push(worker.clone());
                if !worker.ephemeral {
                    work.context["machine"] = json!(worker.machine);
                    work.context["workspace"] = json!(worker.workspace);
                }
            }
            work.jobs.push(serde_json::from_value(json!({"id":ids.next("job"),"worker_id":worker.id,
                "session_id":session,"brief":d.brief.trim(),"join_group":request.inbox_id.to_string(),
                "inbox_id":request.inbox_id,"deliverable":deliverable,"fetch_repo":fetch_repo,"fetch_ref":d.fetch_ref}))?);
        }
    }
    Ok(work)
}
