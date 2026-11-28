//! Durable project state in the owning thread's context. A private timed ask uses
//! the store's indexed due queue and commits in the same transaction as dispatch.
mod graph;
use crate::core::{parent::ParentRequest, worker::Job};
use anyhow::{ensure, Context, Result};
use fridica_core::store::{NewAsk, ObligationChange, Unit};
use graph::Graph;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

const TIMER: &str = "project-review:";
pub const INSTRUCTIONS: &str = "Use manage_project to follow through on owner-delegated outcomes requiring ongoing coordination. Start with a goal and verifiable completion criteria. Read session.projects for status. On a project review, assess evidence, intervene within the original instruction, and delegate as needed. Assignment IDs are stable within the project. Model every prerequisite as an assignment and use its ID in dependencies. Add related assignments in the same operation. Keep prerequisite links after completion. Use dependency_graph.status for effective assignment status and blocked_by for outstanding prerequisites. Report ready for work that can start once prerequisites clear, working for ongoing work, waiting for an expected response, and blocked when intervention is needed. For legacy graph errors, correct the listed links before continuing. Assignment updates replace only the named entries. Jobs delegated with a project operation or during its review are linked automatically. Record material interventions, blockers and closure in the thread. Routine reviews can have a null reply. Track the whole delegated outcome, including follow-through on handoffs. Keep outstanding reviews, checks, blockers and verification as assignments until resolved. Requesting work is a milestone. Report complete only when the assignment has a verified outcome, evidence and verified prerequisites. Close only when every assignment and the overall outcome are verified, or cancel when instructed. The host posts a project notice for each tool action and scheduled review. Use null for manage_project on unrelated turns.";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Assignment {
    id: String,
    owner: String,
    status: String,
    dependencies: Vec<String>,
    evidence: String,
    next_action: String,
    #[serde(default)]
    finished: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Start,
    Update,
    Inspect,
    Pause,
    Resume,
    Close,
    Cancel,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    op: Operation,
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    goal: String,
    #[serde(default)]
    criteria: String,
    #[serde(default)]
    evidence: String,
    #[serde(default)]
    assignments: Vec<Assignment>,
    #[serde(skip)]
    closure_blocker: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Project {
    id: String,
    goal: String,
    instruction: String,
    criteria: String,
    status: String,
    evidence: String,
    assignments: serde_json::Map<String, Value>,
    // Job IDs stay separate from reported assignments. Worker state is read
    // from the supervisor's records when the parent inspects the project.
    jobs: Vec<String>,
    timer: Option<String>,
    next_review_at: Option<f64>,
    last_review_at: Option<f64>,
}

fn estimate_review_interval(_project: &Project, configured_seconds: u32) -> Duration {
    Duration::from_secs(configured_seconds.into())
}

pub fn extend_schema(schema: &mut Value) {
    let string = json!({"type":"string"});
    let strings = json!({"type":"array","items":string});
    let object = |props: Value| {
        let keys: Vec<_> = props.as_object().unwrap().keys().collect();
        json!({"type":"object","properties":props,"required":keys,"additionalProperties":false})
    };
    let assignment = object(
        json!({"id":string,"owner":string,"status":{"type":"string","enum":graph::statuses()},"dependencies":strings,"evidence":string,"next_action":string}),
    );
    let command = object(
        json!({"op":{"type":"string","enum":["start","update","inspect","pause","resume","close","cancel"]},"project_id":string,"goal":string,"criteria":string,"evidence":string,"assignments":{"type":"array","items":assignment}}),
    );
    schema["properties"]["manage_project"] = json!({"anyOf":[command,{"type":"null"}]});
    schema["required"]
        .as_array_mut()
        .unwrap()
        .push(json!("manage_project"));
}

fn owner_instruction<'a>(r: &'a ParentRequest, owner: &str) -> Option<&'a str> {
    match r.trigger["kind"].as_str()? {
        "owner_instruction" => r.trigger["payload"]["text"].as_str(),
        "message"
            if r.trigger["message"]["sender"] == owner
                && r.trigger["message"]["meta"].is_null() =>
        {
            r.trigger["message"]["text"].as_str()
        }
        _ => None,
    }
}

/// Retain the project and its timer when a repair still attempts premature closure.
#[derive(Debug)]
pub struct Incomplete(pub String);
impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "project {} is unfinished", self.0)
    }
}
impl Incomplete {
    pub fn keep_active(&self, reason: String) -> Command {
        Command {
            op: Operation::Update,
            project_id: self.0.clone(),
            evidence: String::new(),
            goal: String::new(),
            criteria: String::new(),
            assignments: vec![],
            closure_blocker: Some(reason),
        }
    }
}

pub fn command(raw: &Value, r: &ParentRequest, owner: &str) -> Result<Option<Command>> {
    let Some(value) = raw.get("manage_project").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let mut c: Command = serde_json::from_value(value.clone())?;
    ensure!(
        r.session["driver"] != "external",
        "project belongs to an external driver"
    );
    ensure!(
        r.session["project_manager"] == owner,
        "only the Slack thread starter can use manage_project"
    );
    ensure!(value.to_string().len() <= 16000, "project update too large");
    let review = r.trigger["project_id"].as_str();
    let source = owner_instruction(r, owner);
    if c.op == Operation::Start {
        ensure!(
            source.is_some()
                && c.project_id.is_empty()
                && !c.goal.trim().is_empty()
                && !c.criteria.trim().is_empty(),
            "start needs an owner instruction, a goal, criteria and an empty project ID"
        );
    } else {
        let p = &r.session["projects"][&c.project_id];
        ensure!(p.is_object(), "unknown project in this thread");
        if c.op != Operation::Inspect {
            ensure!(
                source.is_some() || review == Some(&c.project_id),
                "project change needs an owner instruction or its review"
            );
            let status = p["status"].as_str();
            ensure!(
                c.op != Operation::Resume || status == Some("paused"),
                "resume requires a paused project"
            );
            ensure!(
                c.op != Operation::Cancel || source.is_some(),
                "cancellation requires an owner instruction"
            );
            ensure!(
                status == Some("active")
                    || (status == Some("paused")
                        && matches!(c.op, Operation::Resume | Operation::Cancel)),
                "project is inactive"
            );
        }
    }
    ensure!(
        review.is_none_or(|id| id == c.project_id),
        "review belongs to another project"
    );
    if c.op == Operation::Inspect {
        ensure!(
            c.assignments.is_empty() && c.evidence.is_empty(),
            "inspect is read-only"
        );
    }
    let mut ids = std::collections::HashSet::new();
    for a in &mut c.assignments {
        ensure!(
            !a.id.trim().is_empty()
                && !a.owner.trim().is_empty()
                && !a.status.trim().is_empty()
                && ids.insert(&a.id),
            "assignments need unique IDs, owners and status"
        );
        // Old callers may send descriptive status text and a completion flag.
        let state = graph::reported(&a.status, a.finished);
        if state == "blocked" && a.status.trim().eq_ignore_ascii_case("complete") {
            a.status = state.to_owned();
        }
        a.finished = Some(state == "complete");
    }
    let p = &r.session["projects"][&c.project_id];
    if c.op == Operation::Close {
        validate_graph(&c, p)
            .and_then(|()| verify_completion(&c, p, raw))
            .context(Incomplete(c.project_id.clone()))?;
    } else if !c.assignments.is_empty() {
        validate_graph(&c, p)?;
    }
    Ok(Some(c))
}

fn validate_graph(c: &Command, p: &Value) -> Result<()> {
    let mut assignments = p["assignments"].as_object().cloned().unwrap_or_default();
    for a in &c.assignments {
        assignments.insert(a.id.clone(), json!(a));
    }
    let graph = Graph::build(&assignments)?;
    for a in &c.assignments {
        ensure!(
            a.finished != Some(true) || graph.verified.contains(a.id.as_str()),
            "assignment {} needs evidence and verified prerequisites before it can finish",
            a.id
        );
    }
    if c.op == Operation::Close {
        ensure!(
            graph.verified.len() == assignments.len(),
            "assignments remain open or blocked: {:?}",
            graph
                .status
                .iter()
                .filter(|(_, s)| **s != "complete")
                .collect::<BTreeMap<_, _>>()
        );
    }
    Ok(())
}

fn verify_completion(c: &Command, p: &Value, raw: &Value) -> Result<()> {
    ensure!(
        !c.evidence.trim().is_empty(),
        "successful closure requires evidence of the overall outcome"
    );
    ensure!(
        !p["workers"]
            .as_object()
            .into_iter()
            .flatten()
            .flat_map(|(_, v)| v.as_array().into_iter().flatten())
            .any(|j| matches!(j["status"].as_str(), Some("queued" | "running"))),
        "project jobs are outstanding"
    );
    ensure!(
        raw.get("delegations")
            .or_else(|| raw.get("delegate"))
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty),
        "cannot close while delegating more work"
    );
    Ok(())
}

/// Remove private timer obligations from the model's answerable obligations.
/// The project trigger carries the same inbox identity as the claimed timer.
pub fn attach(u: &mut dyn Unit, r: &mut ParentRequest, owner: &str) -> Result<()> {
    let projects = r.session["context"]
        .as_object_mut()
        .and_then(|v| v.remove("projects"));
    // All accounts consult the same immutable Slack root. An owner instruction,
    // worker report or message claiming a role cannot establish its author.
    let root = r.session["root_ts"]
        .as_str()
        .context("missing project thread root")?;
    let mut starter = r
        .history
        .iter()
        .find(|m| m["ts"] == root)
        .and_then(|m| m["sender"].as_str())
        .map(str::to_owned);
    if starter.is_none() && (projects.is_some() || owner_instruction(r, owner).is_some()) {
        let session = r.session["id"].as_str().context("missing project thread")?;
        starter = u
            .thread_messages(session, -1)?
            .into_iter()
            .find(|m| m.ts == root)
            .map(|m| m.sender);
    }
    let manages = starter.as_deref() == Some(owner);
    if manages || projects.is_some() {
        r.session["project_manager"] = json!(starter);
    }
    let reference = r.trigger["ref"].as_str().unwrap_or("").to_owned();
    let timed = r.trigger["kind"] == "obligation_due" && reference.starts_with(TIMER);
    let timer_live = timed
        && r.obligations.iter().any(|o| {
            o["id"] == reference && matches!(o["state"].as_str(), Some("deferred" | "open"))
        });
    r.obligations
        .retain(|o| !o["id"].as_str().is_some_and(|id| id.starts_with(TIMER)));
    if timed {
        r.trigger["kind"] = json!("project_review");
    }
    let Some(mut projects) = projects else {
        return Ok(());
    };
    let session = r.session["id"].as_str().context("missing project thread")?;
    let jobs: BTreeMap<_, _> = u
        .thread_jobs(session)?
        .into_iter()
        .map(|j| (j.id.clone(), j))
        .collect();
    for (id, p) in projects
        .as_object_mut()
        .context("invalid stored projects")?
    {
        if timer_live && p["timer"] == reference && p["status"] == "active" && starter.is_none() {
            r.trigger["awaiting_thread_starter"] = json!(true);
        }
        if manages
            && p["status"] == "active"
            && ((timer_live && p["timer"] == reference)
                || (matches!(
                    r.trigger["kind"].as_str(),
                    Some("worker_result" | "worker_interrupted" | "worker_progress")
                ) && p["jobs"]
                    .as_array()
                    .is_some_and(|jobs| jobs.iter().any(|j| j == &reference))))
        {
            r.trigger["project_id"] = json!(id);
        }
        let mut workers = BTreeMap::<String, Vec<Value>>::new();
        for job_id in p["jobs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let Some(job) = jobs.get(job_id) else {
                continue;
            };
            let result: Value = job
                .result_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?
                .unwrap_or(Value::Null);
            let observed_at = job.finished_at.max(job.started_at).max(job.queued_at);
            workers.entry(job.worker_id.clone()).or_default().push(json!({
                "job_id":job.id, "assignment":job.brief, "status":job.status, "attempt":job.attempt,
                "observed_at":observed_at, "result":result["status"], "evidence":result["summary"],
                "error":job.error, "latest_progress":p["progress"][job_id]
            }));
        }
        p["workers"] = json!(workers);
        let mut by_owner = BTreeMap::<&str, Vec<&str>>::new();
        for (key, assignment) in p["assignments"].as_object().into_iter().flatten() {
            if let Some(owner) = assignment["owner"].as_str() {
                by_owner.entry(owner).or_default().push(key);
            }
        }
        p["assignments_by_owner"] = json!(by_owner);
        if let Some(assignments) = p["assignments"].as_object() {
            // Older records may contain prose blockers. Keep them visible for
            // repair without interrupting inspection or scheduled reviews.
            p["dependency_graph"] = match Graph::build(assignments) {
                Ok(graph) => json!(graph),
                Err(error) => json!({"error":error.to_string()}),
            };
        }
    }
    r.session["projects"] = projects;
    Ok(())
}

pub fn commit(
    u: &mut dyn Unit,
    r: &ParentRequest,
    command: Option<&Command>,
    jobs: &[Job],
    owner: &str,
    now: f64,
    interval: u32,
) -> Result<()> {
    let review = r.trigger["project_id"].as_str();
    if command.is_some() || review.is_some() {
        ensure!(
            r.session["project_manager"] == owner,
            "project manager mismatch"
        );
    }
    if let Some(c) = command.filter(|c| c.op == Operation::Inspect) {
        let p: Project = serde_json::from_value(r.session["projects"][&c.project_id].clone())?;
        let detail = match Graph::build(&p.assignments) {
            Ok(graph) => {
                let mut lines = Vec::new();
                if !graph.ready.is_empty() {
                    lines.push(format!(
                        "Ready: {}",
                        graph.ready.into_iter().collect::<Vec<_>>().join(", ")
                    ));
                }
                for (id, state) in &graph.status {
                    if !matches!(*state, "ready" | "complete") && !graph.blocked_by.contains_key(id)
                    {
                        lines.push(format!("{id}: {state}"));
                    }
                }
                for (id, blockers) in graph.blocked_by {
                    lines.push(format!("{id} waits for {}", blockers.join(", ")));
                }
                lines.join("\n")
            }
            Err(error) => format!("Dependency graph needs repair: {error}"),
        };
        notice(u, r, &p, "inspected", &detail, owner, now)?;
        if review.is_none() {
            return Ok(());
        }
    }
    let inspected = command.is_some_and(|c| c.op == Operation::Inspect);
    let command = command.filter(|c| c.op != Operation::Inspect);
    let op = command.map(|c| &c.op);
    let session = r.session["id"].as_str().context("missing session")?;
    let id = match command {
        Some(c) if c.op == Operation::Start => format!("project-{}", r.inbox_id),
        Some(c) => c.project_id.clone(),
        None => match review {
            Some(id) => id.to_owned(),
            None => return Ok(()),
        },
    };
    let mut p: Project = if let Some(c) = command.filter(|c| c.op == Operation::Start) {
        Project {
            id: id.clone(),
            goal: c.goal.clone(),
            instruction: owner_instruction(r, owner)
                .context("missing project instruction")?
                .to_owned(),
            criteria: c.criteria.clone(),
            status: "active".into(),
            evidence: String::new(),
            assignments: serde_json::Map::new(),
            jobs: vec![],
            timer: None,
            next_review_at: None,
            last_review_at: None,
        }
    } else {
        serde_json::from_value(r.session["projects"][&id].clone())?
    };
    let previous_status = &r.session["projects"][&id]["dependency_graph"]["status"];
    let mut newly_ready = Vec::new();
    let mut material_change = false;
    if let Some(c) = command {
        for a in &c.assignments {
            if let Some(previous) = p.assignments.get(&a.id) {
                material_change |= previous["dependencies"] != json!(a.dependencies)
                    || previous["status"] != a.status
                    || previous["finished"].as_bool().unwrap_or(false)
                        != (a.finished == Some(true));
            }
            let mut record = json!(a);
            record["reported_at"] = json!(now);
            p.assignments.insert(a.id.clone(), record);
        }
        if !c.assignments.is_empty() {
            let graph = Graph::build(&p.assignments)?;
            newly_ready = graph
                .ready
                .iter()
                .filter(|id| {
                    previous_status
                        .get(**id)
                        .is_some_and(|status| status != "ready")
                })
                .map(|id| id.to_string())
                .collect();
            // Reopening a prerequisite invalidates former completions down the
            // whole chain. Each dependent must supply fresh confirmation.
            let reopened: Vec<String> = graph
                .blocked_by
                .keys()
                .filter(|id| {
                    let a = &p.assignments[**id];
                    graph::reported(
                        a["status"].as_str().unwrap_or_default(),
                        a["finished"].as_bool(),
                    ) == "complete"
                })
                .map(|id| id.to_string())
                .collect();
            for id in reopened {
                p.assignments[&id]["finished"] = json!(false);
                p.assignments[&id]["status"] = json!("ready");
                p.assignments[&id]["reported_at"] = json!(now);
            }
            material_change |= !newly_ready.is_empty() && c.op == Operation::Update;
        }
        if !c.evidence.is_empty() {
            p.evidence.clone_from(&c.evidence);
        }
        p.status = match c.op {
            Operation::Pause => "paused",
            Operation::Resume => "active",
            Operation::Close => "completed",
            Operation::Cancel => "cancelled",
            _ => &p.status,
        }
        .to_owned();
    }
    if review == Some(&id) {
        p.last_review_at = Some(now);
    }
    p.jobs.extend(jobs.iter().map(|j| j.id.clone()));
    if review == Some(&id)
        || material_change
        || matches!(
            op,
            Some(
                Operation::Start
                    | Operation::Resume
                    | Operation::Close
                    | Operation::Pause
                    | Operation::Cancel
            )
        )
    {
        if let Some(timer) = p.timer.take() {
            u.change_obligations(
                session,
                &[ObligationChange {
                    id: timer,
                    state: "answered".into(),
                    details: "{}".into(),
                    due: None,
                }],
                now,
            )?;
        }
        p.next_review_at = None;
        if p.status == "active" {
            let follow_up = !newly_ready.is_empty() && jobs.is_empty();
            let due = if (material_change && review.is_none()) || follow_up {
                now
            } else {
                now + estimate_review_interval(&p, interval).as_secs_f64()
            };
            let timer = format!("{TIMER}{}:{}", id, r.inbox_id);
            u.open_asks(
                session,
                &[NewAsk {
                    id: timer.clone(),
                    source: json!({"project_id":id}).to_string(),
                    summary: format!("Review {}", p.goal),
                    due,
                }],
                now,
            )?;
            p.next_review_at = Some(due);
            p.timer = Some(timer);
        }
    }
    if !inspected {
        let action = match op {
            Some(Operation::Start) => "started",
            Some(Operation::Update) => "updated",
            Some(Operation::Pause) => "paused",
            Some(Operation::Resume) => "resumed",
            Some(Operation::Cancel) => "cancelled",
            Some(Operation::Close) => "completed",
            _ => "reviewed",
        };
        let detail = command
            .map(|c| {
                if let Some(reason) = &c.closure_blocker {
                    return format!("Closure deferred. {reason}");
                }
                let graph = Graph::build(&p.assignments).ok();
                let mut details = c
                    .assignments
                    .iter()
                    .map(|a| {
                        let state = graph
                            .as_ref()
                            .and_then(|g| g.status.get(a.id.as_str()))
                            .copied()
                            .unwrap_or(&a.status);
                        format!("{} ({}) — {}", a.id, a.owner, state)
                    })
                    .collect::<Vec<_>>();
                if !newly_ready.is_empty() {
                    details.push(format!("Ready to proceed: {}", newly_ready.join(", ")));
                }
                if !c.evidence.trim().is_empty() {
                    details.push(c.evidence.clone());
                }
                details.join("\n")
            })
            .unwrap_or_default();
        notice(u, r, &p, action, &detail, owner, now)?;
    }
    u.patch_context(session, &json!({"projects":{id:p}}).to_string())
}

fn notice(
    u: &mut dyn Unit,
    r: &ParentRequest,
    p: &Project,
    action: &str,
    detail: &str,
    owner: &str,
    now: f64,
) -> Result<()> {
    let session = r.session["id"].as_str().context("missing session")?;
    let mut post = super::posts::post(
        format!("{}:project-notice", r.inbox_id),
        session,
        "notice",
        r.session["channel"]
            .as_str()
            .context("missing project channel")?,
        Some(
            r.session["root_ts"]
                .as_str()
                .context("missing project root")?
                .to_owned(),
        ),
    );
    let pending =
        p.assignments.len() - Graph::build(&p.assignments).map_or(0, |g| g.verified.len());
    post.text = format!(
        "Project {} {} · {}. {} open assignments. {}",
        p.id,
        action,
        p.status,
        pending,
        p.goal.chars().take(250).collect::<String>()
    );
    if !detail.is_empty() {
        post.text.push('\n');
        post.text.extend(detail.chars().take(750));
    }
    post.text = post
        .text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    post.meta =
        Some(json!({"kind":"project_notice","owner":owner,"session":session,"project":p.id,"v":2}));
    u.queue_post(&post, now)?;
    Ok(())
}

/// A member of a joined job group finished. Keep the normal joined report and
/// bring the manager's deadline forward once for all intervening completions.
pub fn wake_pending(u: &mut dyn Unit, r: &ParentRequest, now: f64) -> Result<()> {
    let id = r.trigger["project_id"]
        .as_str()
        .context("missing project")?;
    let p = &r.session["projects"][id];
    if p["next_review_at"].as_f64().is_some_and(|due| due > now) {
        let session = r.session["id"].as_str().context("missing session")?;
        let timer = p["timer"].as_str().context("missing project timer")?;
        if u.change_obligations(
            session,
            &[ObligationChange {
                id: timer.into(),
                state: "deferred".into(),
                details: "{}".into(),
                due: Some(now),
            }],
            now,
        )? {
            u.patch_context(
                session,
                &json!({"projects":{id:{"next_review_at":now}}}).to_string(),
            )?;
        }
    }
    Ok(())
}

/// A finished job can reach SQLite while the parent is deciding. Its result
/// inbox now owns the next decision, so the timer yields before dispatch.
pub fn result_arrived(u: &mut dyn Unit, request: &ParentRequest) -> Result<bool> {
    if request.trigger["kind"] != "project_review" {
        return Ok(false);
    }
    let id = request.trigger["project_id"]
        .as_str()
        .context("missing project ID")?;
    let workers = &request.session["projects"][id]["workers"];
    for seen in workers
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(_, jobs)| jobs.as_array().into_iter().flatten())
    {
        if matches!(seen["status"].as_str(), Some("running" | "queued")) {
            let job = u.job_record(seen["job_id"].as_str().context("missing job ID")?)?;
            if !matches!(job.status.as_str(), "running" | "queued") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimator_uses_the_configured_interval() {
        let project: Project = serde_json::from_value(json!({
            "id":"project-17","instruction":"Track this","goal":"Deliver","criteria":"Receipt",
            "status":"active","assignments":{},"jobs":[],"evidence":""
        }))
        .unwrap();
        assert_eq!(
            estimate_review_interval(&project, crate::config::schema::default_review_interval()),
            Duration::from_secs(900)
        );
        assert_eq!(
            estimate_review_interval(&project, 45),
            Duration::from_secs(45)
        );
    }
}
