//! The one unit of work a decided turn commits: its reply, files, workers,
//! jobs, hand-offs, obligation changes and audit records, or nothing at all
//! if the thread moved on while the parent was deciding.
use super::actor::{handoff_hop, hold_clears, Call, Step, HANDOFF_CONTEXT};
use crate::{
    attention::{self, Answer},
    core::parent::{Decision, Disposition, NoteKind, ParentRequest, ReplyStatus},
};
use anyhow::{bail, Context, Result};
use fridica_core::store::{
    Arrival, Fence, NewAsk, ObligationChange, ParentTurn, QueuedHandoff, Store as Backend,
    TurnClose, Unit,
};
use serde_json::{json, Value};

/// What a turn decided, ready to commit.
pub(super) struct Outcome {
    pub(super) request: ParentRequest,
    pub(super) decision: Decision,
    pub(super) project: Option<super::projects::Command>,
    pub(super) work: super::delegation::Work,
    pub(super) calls: Vec<Call>,
    pub(super) verdict: String,
    pub(super) turn: u64,
    /// The channel's cooldown, when this would be its first unasked reply.
    pub(super) cooldown: Option<f64>,
}
/// The actor's settings a commit applies.
pub(super) struct Rules {
    pub(super) owner: String,
    pub(super) now: f64,
    /// The wait or no-progress streak that opens a signal.
    pub(super) signal: usize,
    pub(super) review_interval: u32,
    pub(super) reply_limit: usize,
}
/// The thread's streak counters and status, as this turn leaves them.
struct Progress {
    waiting: i64,
    quiet: i64,
    status: String,
    hash: String,
}
pub(super) fn is_result(request: &ParentRequest) -> bool {
    matches!(
        request.trigger["kind"].as_str(),
        Some("worker_result" | "worker_interrupted")
    )
}
/// The Slack event that started the turn, when a message did.
pub(super) fn event_id(request: &ParentRequest) -> Option<&str> {
    request.trigger["message"]["event_id"].as_str()
}

/// Whether the thread moved on while the parent was deciding: the turn then
/// commits nothing and runs again on the new state.
fn superseded(
    u: &mut dyn Unit,
    id: i64,
    session: &str,
    request: &ParentRequest,
    decision: &Decision,
    owner: &str,
) -> Result<bool> {
    let Fence { version, active } = u.fence_turn(session, id)?;
    let result_stale = if is_result(request) {
        super::results::load(u, session, request.trigger["ref"].as_str().unwrap_or(""))?["results"]
            != request.trigger["results"]
    } else {
        false
    };
    let notes_changed = super::effects::notes(u, session)?.0
        != request.session["notes"]["revision"].as_i64().unwrap_or(0);
    let controls_current = u.worker_controls_current(request, &decision.worker_control)?;
    // A message that arrived while the parent was deciding (not one of our
    // own echoes) makes the turn stale once, so it reruns and sees it.
    let seen = request
        .history
        .iter()
        .chain(std::iter::once(&request.trigger["message"]))
        .filter_map(|m| m["ts"].as_str()?.parse::<f64>().ok())
        .fold(f64::NEG_INFINITY, f64::max);
    let Arrival { arrived, reread } =
        u.arrival(session, id, seen.is_finite().then_some(seen), owner)?;
    Ok(Some(version) != request.session["version"].as_i64()
        || !active
        || result_stale
        || notes_changed
        || !controls_current
        || (arrived && !reread))
}

/// The worker(s) a report is about, failed ones first, each named once.
fn reported_workers(request: &ParentRequest) -> String {
    let results = request.trigger["results"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let failed = |r: &Value| r["job_status"] != "done" || r["result"]["status"] == "failed";
    let mut ids: Vec<&str> = vec![];
    for id in results
        .iter()
        .filter(|r| failed(r))
        .chain(results.iter().filter(|r| !failed(r)))
        .filter_map(|r| r["worker_id"].as_str())
    {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.join(",").chars().take(64).collect()
}

/// The metadata a reply is posted with.
fn reply_meta(
    request: &ParentRequest,
    session: &str,
    owner: &str,
    turn: u64,
    status: &str,
) -> Value {
    let report = is_result(request);
    let worker = if report {
        reported_workers(request)
    } else {
        String::new()
    };
    // The PRs its jobs were dispatched to review, or a person's ask (no agent
    // meta) names: only these are signed.
    let review: Vec<String> = request.trigger["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["brief"].as_str())
        .chain(
            request.trigger["message"]["text"]
                .as_str()
                .filter(|_| request.trigger["message"]["meta"].is_null()),
        )
        .flat_map(super::verdict::subjects)
        .collect();
    let mut meta = json!({"owner":owner,"session":session,"turn":turn,"status":status,
        "kind":if report {"report"} else {"reply"},"worker":worker,"v":2});
    if request.session["github_reviews"] == true {
        meta["github_reviews"] = json!(true);
    } else if !review.is_empty() {
        meta["review"] = json!(review);
    }
    meta
}

/// Queue the turn's reply unless it only repeats the last one. Returns
/// whether it was queued, with the streak counters and status moved on.
#[allow(clippy::too_many_arguments)]
fn queue_reply(
    u: &mut dyn Unit,
    id: i64,
    session: &str,
    outcome: &Outcome,
    reply: &crate::core::parent::Reply,
    explicit_repeat: bool,
    rules: &Rules,
    progress: &mut Progress,
) -> Result<bool> {
    let (request, decision) = (&outcome.request, &outcome.decision);
    let candidate = crate::core::policy::reply_hash(&reply.text);
    let duplicate = candidate == progress.hash
        && reply.status.as_str() == progress.status
        && !explicit_repeat
        && reply.answers.is_empty()
        && reply.details.is_empty()
        && outcome.work.jobs.is_empty()
        && !is_result(request)
        && decision.note.kind != NoteKind::Correction;
    if duplicate {
        return Ok(false);
    }
    let answer = Answer {
        key: format!("{id}:reply"),
        session: session.into(),
        channel: request.session["channel"]
            .as_str()
            .context("missing channel")?
            .into(),
        thread_ts: request.session["root_ts"]
            .as_str()
            .context("missing root timestamp")?
            .into(),
        text: reply.text.clone(),
        obligations: reply.answers.clone(),
        inbox: id,
    };
    let post = attention::queue_answer_tx(u, &answer, rules.now)?;
    // A post a hand-off asked for settles the source thread's asks it names
    // once delivered (#108). A blocked notice settles nothing, here or there.
    if request.trigger["kind"] == "handoff" && !matches!(reply.status, ReplyStatus::Blocked) {
        let from = request.trigger["payload"]["from_session"]
            .as_str()
            .unwrap_or("");
        let answers: Vec<String> = request.trigger["payload"]["answers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        u.answer_for_handoff(from, &answers, post, rules.now)?;
    }
    let meta = reply_meta(
        request,
        session,
        &rules.owner,
        outcome.turn,
        reply.status.as_str(),
    );
    let trigger_event = event_id(request)
        .or_else(|| request.trigger["origin"]["event_id"].as_str())
        .or_else(|| request.trigger["refused"]["trigger_event"].as_str())
        .unwrap_or("");
    u.label_post(post, &meta.to_string(), trigger_event, is_result(request))?;
    if outcome.cooldown.is_some() {
        u.mark_unsolicited(
            request.session["workspace"].as_str(),
            request.session["channel"].as_str(),
            rules.now,
        )?;
    }
    if candidate != progress.hash && decision.note.kind != NoteKind::Ack {
        progress.quiet = 0;
    }
    progress.hash = candidate;
    progress.waiting = if matches!(reply.status, ReplyStatus::Waiting) {
        progress.waiting + 1
    } else {
        0
    };
    progress.status = reply.status.as_str().into();
    Ok(true)
}

/// Hand-offs (#108): the target thread's own turn acts on each, with this
/// thread's state as of this turn attached as a fork bundle.
fn queue_handoffs(
    u: &mut dyn Unit,
    id: i64,
    session: &str,
    request: &ParentRequest,
    decision: &Decision,
    now: f64,
) -> Result<()> {
    if decision.handoffs.is_empty() {
        return Ok(());
    }
    let class = u.handoff_class(id)?;
    let (workspace, channel) = (
        request.session["workspace"].as_str().unwrap_or(""),
        request.session["channel"].as_str().unwrap_or(""),
    );
    let handoffs: Vec<QueuedHandoff> = decision
        .handoffs
        .iter()
        .enumerate()
        .map(|(index, handoff)| {
            let target = format!("{workspace}:{channel}:{}", handoff.thread);
            // This thread's state for the target, without the target's own.
            let mut source = request.clone();
            if let Some(linked) = source.session["linked_threads"].as_array_mut() {
                linked.retain(|t| t["thread"] != handoff.thread.as_str());
            }
            let bundle = crate::core::fork::render(&crate::core::fork::snapshot(
                &source,
                decision,
                HANDOFF_CONTEXT,
            ));
            let payload = json!({"from":request.session["root_ts"],"from_session":session,
                "kind":handoff.kind,"note":handoff.note.trim(),"answers":handoff.answers,
                "class":class,"hop":handoff_hop(request)+1,"context":bundle});
            let details = |queued: bool| {
                json!({"from":session,"inbox_id":id,"kind":handoff.kind,"queued":queued})
                    .to_string()
            };
            QueuedHandoff {
                target,
                from: session.into(),
                payload: payload.to_string(),
                dedup_key: format!("handoff:{id}:{index}"),
                queued: details(true),
                skipped: details(false),
            }
        })
        .collect();
    u.queue_handoffs(&handoffs, now)
}

/// The turn's parent calls as the audit keeps them. A failed turn posts
/// nothing and blocks the thread, which puts it in the owner's attention
/// view; the audit keeps the cause.
fn parent_turns(calls: &[Call], id: i64) -> Vec<ParentTurn> {
    calls
        .iter()
        .map(|call| {
            let error = call.settlement_error.unwrap_or("");
            let mut context = call.context();
            if let Some(validation) = &call.validation_error {
                context["validation_error"] = json!(validation);
            }
            let blocked = (!error.is_empty()).then(|| {
                json!({"inbox_id":id,"reason":error,"failure":call.failure,
                    "validation_error":call.validation_error})
                .to_string()
            });
            call.turn(&context, error, blocked)
        })
        .collect()
}

pub(super) async fn commit(
    store: &impl Backend,
    id: i64,
    session: String,
    mut outcome: Outcome,
    rules: Rules,
) -> Result<Step> {
    let sql_turn = i64::try_from(outcome.turn).context("turn counter exceeds supported range")?;
    store
        .transact(move |u| {
            let now = rules.now;
            // A completion can also bump the thread version. Yield to its result
            // before the ordinary stale-turn path requeues this older timer.
            let request = &outcome.request;
            if request.trigger["kind"] == "project_review"
                && u.thread_active(&session)?
                && u.inbox_attempts(id)?.1 == "processing"
                && super::projects::result_arrived(u, request)?
            {
                u.defer_turn(id, now + 1.)?;
                return Ok(Step::Deferred);
            }
            if superseded(
                u,
                id,
                &session,
                &outcome.request,
                &outcome.decision,
                &rules.owner,
            )? {
                u.return_turn(id)?;
                return Ok(Step::Stale);
            }

            if let Some(cooldown) = outcome
                .cooldown
                .filter(|_| outcome.decision.reply.is_some())
            {
                let last = u.last_unsolicited(
                    request.session["workspace"].as_str(),
                    request.session["channel"].as_str(),
                )?;
                if let Some(last) = last.filter(|last| now - last < cooldown) {
                    u.defer_turn(id, last + cooldown)?;
                    return Ok(Step::Deferred);
                }
            }
            let mut progress = Progress {
                waiting: request.session["wait_streak"].as_i64().unwrap_or(0),
                quiet: request.session["no_progress"].as_i64().unwrap_or(0)
                    + i64::from(request.trigger["kind"] != "project_review"),
                status: request.session["status"]
                    .as_str()
                    .unwrap_or("new")
                    .to_string(),
                hash: request.session["last_reply_hash"]
                    .as_str()
                    .unwrap_or("")
                    .to_owned(),
            };
            if outcome.decision.reopen_blocked && progress.status == "blocked" {
                progress.status = "complete".into();
            }
            let repeat = super::replies::repeat_evidence(u, request, &rules.owner)?;
            // Only the stand-in for a failed parent turn reaches here unsent: it
            // settles the thread's status without posting anything.
            if let Some(reply) = outcome.decision.reply.as_ref().filter(|r| !r.send) {
                progress.status = reply.status.as_str().into();
            }
            let files = super::results::Files::load(u, request, &session)?;
            if let Some(reply) = outcome.decision.reply.as_mut().filter(|r| r.send) {
                files.describe(reply);
                super::replies::render(
                    u,
                    request,
                    &rules.owner,
                    reply,
                    rules.reply_limit,
                    outcome.project.is_some(),
                )?;
            }
            let decision = &outcome.decision;
            let mut posted = None;
            if let Some(reply) = decision.reply.as_ref().filter(|r| r.send) {
                let explicit = repeat["allowed"] == true;
                if queue_reply(
                    u,
                    id,
                    &session,
                    &outcome,
                    reply,
                    explicit,
                    &rules,
                    &mut progress,
                )? {
                    posted = Some(reply);
                }
            }
            // A parent-error stand-in is an explicit hold on publication. A
            // successful turn with no text still delivers its approved files.
            if decision.reply.as_ref().is_none_or(|r| r.send) {
                files.queue(u, request, posted, &session, id, now)?;
            }
            let work = &outcome.work;
            u.add_workers(&work.workers, now)?;
            u.queue_jobs(&work.jobs, now)?;
            super::projects::commit(
                u,
                request,
                outcome.project.as_ref(),
                &work.jobs,
                &rules.owner,
                now,
                rules.review_interval,
            )?;
            u.queue_worker_controls(request, &decision.worker_control, now)?;
            if is_result(request) {
                let jobs: Vec<Option<String>> = request.trigger["results"]
                    .as_array()
                    .context("missing results")?
                    .iter()
                    .map(|r| r["id"].as_str().map(str::to_owned))
                    .collect();
                u.mark_reported(&session, &jobs)?;
            }
            if u.jobs_running(&session)? && progress.status != "blocked" {
                progress.status = "working".into();
            }
            if !work.jobs.is_empty() {
                if decision.note.kind != NoteKind::Ack {
                    progress.quiet = 0;
                }
                progress.waiting = 0;
            }
            if !work.context.is_null() {
                u.patch_context(&session, &work.context.to_string())?;
            }
            super::effects::commit(
                u,
                decision,
                &session,
                id,
                now,
                hold_clears(request, decision),
            )?;
            queue_handoffs(u, id, &session, request, decision, now)?;
            let changes = decision
                .dispositions
                .iter()
                .map(|disposition| {
                    let (state, due) = match disposition {
                        Disposition::Declined { .. } => ("declined", None),
                        Disposition::Deferred { until, .. } => ("deferred", Some(*until)),
                    };
                    Ok(ObligationChange {
                        id: disposition.id().into(),
                        state: state.into(),
                        details: serde_json::to_string(disposition)?,
                        due,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            if !u.change_obligations(&session, &changes, now)? {
                bail!("obligation changed before actor commit");
            }
            let asks: Vec<NewAsk> = decision
                .asks
                .iter()
                .enumerate()
                .map(|(index, ask)| NewAsk {
                    id: format!("ask:{id}:{index}"),
                    source:
                        json!({"inbox_id":id,"event_id":request.trigger["message"]["event_id"]})
                            .to_string(),
                    summary: ask.summary.clone(),
                    due: ask.due,
                })
                .collect();
            u.open_asks(&session, &asks, now)?;
            let signal = rules.signal as i64;
            if request.trigger["kind"] != "project_review"
                && (progress.waiting == signal || progress.quiet == signal)
            {
                let key = format!("streak:{session}:{id}");
                u.open_streak_signal(&key, &session, &json!({"inbox_id":id}).to_string(), now)?;
            }
            let mut action = serde_json::to_value(decision)?;
            if let Some(project) = &outcome.project {
                action["manage_project"] = json!(project);
            }
            u.record_parent_calls(
                &session,
                id,
                &action.to_string(),
                &parent_turns(&outcome.calls, id),
                now,
            )?;
            u.close_turn(&TurnClose {
                session: session.clone(),
                status: progress.status,
                reply_key: format!("{id}:reply"),
                turn: sql_turn,
                waiting: progress.waiting,
                quiet: progress.quiet,
                hash: progress.hash,
                summary: decision.summary.clone(),
                now,
            })?;
            u.finish_turn(id, event_id(request), &outcome.verdict)?;
            u.record(
                "actor_commit",
                now,
                &json!({"inbox_id":id,"request":request,"decision":action,
                    "workers":work.workers,"jobs":work.jobs,"reply_repeat":repeat})
                .to_string(),
                true,
            )?;
            Ok(Step::Committed)
        })
        .await
}
