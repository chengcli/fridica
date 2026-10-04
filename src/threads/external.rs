//! The external-driver surface (fridica#130): a driver on the control socket
//! starts a thread's work and posts in its thread without a parent turn.
//!
//! A delegation reuses the parent's validation and placement
//! ([`delegation::prepare`]) and commits its workers and jobs in one unit of
//! work. It never calls the actor's commit and never reserves a reply, so the
//! thread's streaks and reply budget are untouched. A post goes through the
//! outbox and its egress gate like any post, but reserves nothing either: a
//! paused thread does not swallow it.
use super::delegation::{self, Scope};
use crate::{
    config::Config,
    core::{
        fork::ContextMode,
        parent::{Decision, Delegation, ParentRequest},
        time::Identifiers,
    },
    slack::names::Names,
    store::Shared,
};
use anyhow::Result;
use fridica_core::store::{Store as _, Unit};
use serde_json::{json, Value};
use std::sync::Arc;

/// A refused request: its HTTP status and stable error code.
pub type Refusal = (u16, &'static str);

/// One delegation a driver asks for. `tags` are its own correlation labels,
/// stored on the job and echoed on its views, never machine selectors.
#[derive(Clone, Debug)]
pub struct DelegateRequest {
    pub role: String,
    pub brief: String,
    pub context: ContextMode,
    /// A live worker of the thread to resume, or empty for a new one.
    pub worker_id: String,
    pub ephemeral: bool,
    /// `same` (the machine's default backend), `other` (another configured
    /// backend) or a backend's name.
    pub backend: String,
    pub deliverable: String,
    pub tags: Vec<String>,
}

/// What a delegation started: `{join_group, jobs: [{job_id, worker_id, role}]}`.
pub async fn delegate(
    store: &Shared,
    config: Arc<Config>,
    ids: Arc<dyn Identifiers>,
    session: String,
    request: DelegateRequest,
    now: f64,
) -> Result<std::result::Result<Value, Refusal>> {
    store
        .transact(move |u| {
            let Some(view) = u.thread(&session)? else {
                return Ok(Err((404, "no_such_thread")));
            };
            if matches!(view.control.as_str(), "closed" | "archived" | "cleaned") {
                return Ok(Err((409, "thread_closed")));
            }
            let parent = turn_request(u, &session, &view)?;
            let backend = match backend(&request.backend, &config, &parent.session) {
                Ok(backend) => backend,
                Err(refusal) => return Ok(Err(refusal)),
            };
            let decision = Decision {
                delegations: vec![Delegation {
                    brief: request.brief.clone(),
                    worker_id: request.worker_id.clone(),
                    backend,
                    role: request.role.clone(),
                    ephemeral: request.ephemeral,
                    deliverable: request.deliverable.clone(),
                    context: request.context,
                    ..Delegation::default()
                }],
                ..Decision::default()
            };
            let scope = Scope {
                allowed: config.slack.channels.contains(&view.channel)
                    && config.slack.may_delegate(&view.channel),
                limits: &config.limits,
                machines: &config.machines,
                roles: crate::config::roles::worker_roles(),
            };
            let mut work = match delegation::prepare(&decision, &parent, Some(scope), Some(&*ids))
            {
                Ok(work) => work,
                Err(error) => return Ok(Err(refusal(&error))),
            };
            // Each delegation is its own join group, with no inbox item behind it.
            let group = ids.next("group");
            for job in &mut work.jobs {
                job.join_group = group.clone();
                job.inbox_id = None;
                job.tags = request.tags.clone();
            }
            let roles: Vec<(String, String)> = parent.session["work"]["workers"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|w| (text(&w["id"]), text(&w["role"])))
                .chain(work.workers.iter().map(|w| (w.id.clone(), w.role.clone())))
                .collect();
            let jobs: Vec<Value> = work
                .jobs
                .iter()
                .map(|j| {
                    let role = roles
                        .iter()
                        .find(|(id, _)| *id == j.worker_id)
                        .map_or("general", |(_, role)| role.as_str());
                    json!({"job_id":j.id,"worker_id":j.worker_id,"role":role})
                })
                .collect();
            u.add_workers(&work.workers, now)?;
            u.queue_jobs(&work.jobs, now)?;
            if !work.context.is_null() {
                u.patch_context(&session, &work.context.to_string())?;
            }
            u.record(
                "external_delegate",
                now,
                &json!({"session":session,"join_group":group,"workers":work.workers,"jobs":work.jobs})
                    .to_string(),
                true,
            )?;
            Ok(Ok(json!({"join_group":group,"jobs":jobs})))
        })
        .await
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or("").to_owned()
}

/// The thread as a turn would see it, as far as placement and a fork
/// snapshot read it. There is no inbox item and no trigger.
fn turn_request(
    u: &mut dyn Unit,
    session: &str,
    view: &fridica_core::store::ThreadView,
) -> Result<ParentRequest> {
    let (revision, notes) = super::effects::notes(u, session)?;
    let files = u.delegable_files(session)?;
    let history = u
        .thread_messages(session, 60)?
        .into_iter()
        .map(|m| {
            let meta = m
                .meta_json
                .as_deref()
                .map(serde_json::from_str::<Value>)
                .transpose()?
                .unwrap_or(Value::Null);
            Ok(json!({"ts":m.ts,"sender":m.sender,"text":m.text,"meta":meta}))
        })
        .collect::<Result<Vec<Value>>>()?;
    Ok(ParentRequest {
        inbox_id: 0,
        call: "delegate".into(),
        session: json!({
            "id":session,"workspace":view.workspace,"channel":view.channel,"root_ts":view.root_ts,
            "status":view.status,"turns":view.turns,"summary":view.summary,
            "context":serde_json::from_str::<Value>(&view.context_json)?,
            "decisions":serde_json::from_str::<Value>(&view.decisions_json)?,
            "notes":{"revision":revision,"data":notes},
            "work":u.work_context(session)?,"files":files,
        }),
        trigger: json!({"kind":"external"}),
        history,
        obligations: vec![],
        linked: vec![],
        github_state: vec![],
        previous: None,
        errors: vec![],
    })
}

/// The backend a driver's `same`, `other` or named backend asks placement for.
/// `same` is the machine's default (what a parent delegation without a
/// backend gets); `other` is the first other backend configured, on the
/// thread's machine if it has one.
fn backend(choice: &str, config: &Config, session: &Value) -> std::result::Result<String, Refusal> {
    match choice {
        "" | "same" => Ok(String::new()),
        "other" => {
            let registry = &config.machines;
            let sticky = session["context"]["machine"].as_str().unwrap_or("");
            let home = registry
                .get(sticky)
                .or_else(|| registry.get(&registry.default))
                .ok_or((409, "no_other_backend"))?;
            home.backends
                .iter()
                .chain(registry.machines.iter().flat_map(|m| m.backends.iter()))
                .find(|b| **b != home.default_backend)
                .cloned()
                .ok_or((409, "no_other_backend"))
        }
        name => Ok(name.to_owned()),
    }
}

/// A stable code for each way [`delegation::prepare`] refuses.
fn refusal(error: &anyhow::Error) -> Refusal {
    if error
        .downcast_ref::<fridica_core::placement::MatchError>()
        .is_some()
    {
        return (400, "placement");
    }
    let message = error.to_string();
    let has = |text: &str| message.contains(text);
    if has("too many persistent workers") {
        (409, "slots")
    } else if has("invalid worker role") {
        (400, "invalid_role")
    } else if has("needs a brief") {
        (400, "invalid_brief")
    } else if has("invalid deliverable") {
        (400, "invalid_deliverable")
    } else if has("worker_id is not a live worker") {
        (404, "unknown_worker")
    } else if has("delegation is disabled") || has("delegation is not configured") {
        (403, "delegation_disabled")
    } else if has("no longer configured") {
        (400, "placement")
    } else {
        (400, "invalid_delegation")
    }
}

/// The post kinds a driver may send, and the outbox kind each is queued as.
/// A driver's `report` is not the parent's report: a refusal of it gives the
/// parent no rewrite turn.
pub fn outbox_kind(kind: &str) -> Option<&'static str> {
    match kind {
        "study_claim" => Some("study_claim"),
        "study_result" => Some("study_result"),
        "study_root" => Some("study_root"),
        "report" => Some("driver_report"),
        _ => None,
    }
}

/// Where a driver's post goes.
#[derive(Clone, Debug)]
pub enum Target {
    /// Into a thread; a `study_root` starts a new thread in its channel.
    Thread(String),
    /// A new root in a configured channel (by ID or recorded name).
    Channel(String),
}

/// One post a driver asks for.
#[derive(Clone, Debug)]
pub struct PostRequest {
    pub kind: String,
    pub status: String,
    pub text: String,
    pub details: String,
    /// The driver's idempotency key, if any.
    pub client_id: Option<String>,
}

/// Queue a driver's post: `{outbox_id}`, and for a root post already sent
/// (a repeated `client_id`) also the thread it started.
pub async fn post(
    store: &Shared,
    config: Arc<Config>,
    ids: Arc<dyn Identifiers>,
    target: Target,
    request: PostRequest,
    now: f64,
) -> Result<std::result::Result<Value, Refusal>> {
    let Some(kind) = outbox_kind(&request.kind) else {
        return Ok(Err((400, "invalid_post_kind")));
    };
    store
        .transact(move |u| {
            let (session, workspace, channel, root, turn) = match &target {
                Target::Thread(id) => {
                    let Some(view) = u.thread(id)? else {
                        return Ok(Err((404, "no_such_thread")));
                    };
                    (
                        id.clone(),
                        view.workspace,
                        view.channel,
                        Some(view.root_ts),
                        view.turns,
                    )
                }
                Target::Channel(reference) => {
                    let names = Names::recorded(u, &config.slack)?;
                    let Some(channel) = names.channel_id(reference) else {
                        return Ok(Err((404, "unknown_channel")));
                    };
                    let workspace = config.slack.workspace.clone();
                    // A root has no thread yet: its post is kept under the
                    // channel's own placeholder thread.
                    (
                        format!("{workspace}:{channel}:channel"),
                        workspace,
                        channel,
                        None,
                        0,
                    )
                }
            };
            if !config.slack.channels.contains(&channel) {
                return Ok(Err((404, "unknown_channel")));
            }
            let root_post = request.kind == "study_root";
            let thread_ts = if root_post { None } else { root.clone() };
            let key = match &request.client_id {
                Some(id) => format!("driver:{session}:{id}"),
                None => format!("driver:{}", ids.next("post")),
            };
            let meta = json!({"owner":config.owner.slack_user,"session":session,"turn":turn,
                "status":request.status,"kind":request.kind,"worker":"","v":2});
            let post = crate::core::delivery::Post {
                idem_key: key.clone(),
                session_id: session.clone(),
                kind: kind.into(),
                channel: channel.clone(),
                thread_ts: thread_ts.clone(),
                text: request.text.clone(),
                meta: Some(meta),
                filename: String::new(),
                blob: None,
                after: String::new(),
            };
            let id = match u.queue_post(&post, now) {
                Ok(id) => id,
                Err(error) if error.to_string().contains("reused with different content") => {
                    return Ok(Err((409, "client_id_conflict")));
                }
                Err(error) => return Err(error),
            };
            if !request.details.is_empty() {
                u.queue_post(
                    &crate::core::delivery::Post {
                        idem_key: format!("{key}:details"),
                        session_id: session.clone(),
                        kind: "upload".into(),
                        channel: channel.clone(),
                        thread_ts,
                        text: String::new(),
                        meta: None,
                        filename: format!("details-{id}.md"),
                        blob: Some(request.details.as_bytes().to_vec()),
                        after: key,
                    },
                    now,
                )?;
            }
            let mut answer = json!({"outbox_id":id});
            if root_post {
                // Known only once delivered: a new root's Slack timestamp is
                // its thread. The driver otherwise learns it from the echoed
                // `message` event.
                let sent = u
                    .thread_posts(&session)?
                    .into_iter()
                    .find(|p| p.id == id)
                    .map(|p| p.sent_ts)
                    .filter(|ts| !ts.is_empty());
                if let Some(ts) = sent {
                    let thread = format!("{workspace}:{channel}:{ts}");
                    answer["thread_id"] = json!(thread);
                    answer["thread"] = json!(thread);
                }
            }
            Ok(Ok(answer))
        })
        .await
}
