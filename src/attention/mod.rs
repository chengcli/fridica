//! Durable attention operations. All decisions that reserve capacity or satisfy
//! an obligation share a transaction with the inbox/outbox effects they describe.
pub mod backfill;
use crate::{
    config::Attention,
    core::{ids::ThreadId, Authority},
};
use anyhow::{bail, Context, Result};
use fridica_core::store::{
    ArrivedMessage, Disposal, Mention, QueuedAnswer, Store as Backend, Unit,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub event_id: String,
    pub workspace: String,
    pub channel: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub sender: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<Value>,
    #[serde(default = "socket_source")]
    pub source: String,
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub attachments: Vec<Value>,
}
fn socket_source() -> String {
    "socket".into()
}
impl Message {
    pub fn session_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.workspace,
            self.channel,
            self.thread_ts.as_ref().unwrap_or(&self.ts)
        )
    }
}

#[derive(Debug, PartialEq)]
pub enum Capacity {
    Reserved,
    Deferred(f64),
}

pub async fn intake(
    store: &impl Backend,
    msg: Message,
    owner: String,
    now: f64,
    grace: f64,
    obligation_id: String,
) -> Result<Option<i64>> {
    store
        .transact(move |u| intake_tx(u, msg, &owner, now, grace, &obligation_id, None))
        .await
}

/// Shared atomic intake for direct callers, Socket Mode and catch-up. A catch-up
/// cutoff decides history-only status against the thread in this transaction.
pub(crate) fn intake_tx(
    u: &mut dyn Unit,
    msg: Message,
    owner: &str,
    now: f64,
    grace: f64,
    obligation_id: &str,
    history_before: Option<f64>,
) -> Result<Option<i64>> {
    if !now.is_finite() || !grace.is_finite() || grace <= 0. {
        bail!("invalid intake time or grace");
    }
    msg.session_id()
        .parse::<ThreadId>()
        .map_err(anyhow::Error::msg)?;
    let session = msg.session_id();
    // A message in an archived thread brings the thread back first (#114).
    u.revive_or_note(&session, now)?;
    let mentioned = msg.text.contains(&format!("<@{owner}>"));
    let mut work = true;
    let mut created = now;
    if let Some(cutoff) = history_before {
        let timestamp: f64 = msg.ts.parse()?;
        if !timestamp.is_finite() || !cutoff.is_finite() {
            bail!("invalid catch-up time");
        }
        let waiting = u.thread_waiting(&session)?;
        work = timestamp >= cutoff || mentioned || waiting;
        if !work {
            created = now.min(timestamp);
        }
    }
    let mut record =
        json!({"message":msg,"owner":owner,"grace":grace,"obligation_id":obligation_id});
    if history_before.is_some() {
        record["work"] = json!(work);
    }
    u.record("intake", now, &record.to_string(), true)?;
    let root = msg.thread_ts.as_ref().unwrap_or(&msg.ts);
    let inserted = u.keep_message(&ArrivedMessage {
        event_id: msg.event_id.clone(),
        workspace: msg.workspace.clone(),
        channel: msg.channel.clone(),
        ts: msg.ts.clone(),
        root_ts: root.clone(),
        thread_ts: msg.thread_ts.clone(),
        sender: msg.sender.clone(),
        text: msg.text.clone(),
        files: serde_json::to_string(&msg.files)?,
        source: msg.source.clone(),
        meta: msg.meta.as_ref().map(|m| m.to_string()),
        received_at: now,
        attachments: serde_json::to_string(&msg.attachments)?,
        mentions_owner: mentioned,
    })?;
    if !inserted {
        return Ok(None);
    }
    u.open_thread(&session, &msg.workspace, &msg.channel, root, created)?;
    // The channel ledger (#108): what this message refers to.
    u.record_links(&msg.workspace, &msg.channel, &session, &msg.text, created)?;
    if msg.source == "self" || !work {
        return Ok(None);
    }
    let inbox = u.queue_message(&session, &msg.event_id, now)?;
    if mentioned && msg.sender != owner {
        u.open_mention(&Mention {
            id: obligation_id.into(),
            session,
            dedup_key: format!("mention:{}:{}:{}", msg.workspace, msg.channel, msg.ts),
            source: json!({"event_id":msg.event_id}).to_string(),
            created: now,
            due: now + grace,
        })?;
    }
    Ok(Some(inbox))
}

pub async fn reserve(
    store: &impl Backend,
    session: String,
    inbox: i64,
    trigger: String,
    now: f64,
    id: String,
    limits: Attention,
) -> Result<Capacity> {
    limits.validate()?;
    if !matches!(trigger.as_str(), "owner" | "peer" | "human") || !now.is_finite() {
        bail!("invalid reply reservation");
    }
    store
        .transact(move |u| {
            if !u.thread_active(&session)? {
                bail!("thread is paused or closed; reply reservation refused");
            }
            if !u.inbox_open(inbox, &session)? {
                bail!("inbox item is not available in this session");
            }
            let prior = u.reservation_state(inbox)?;
            if prior.as_deref() == Some("reserved") {
                return Ok(Capacity::Reserved);
            }
            if prior.as_deref() == Some("sent") {
                bail!("inbox item already replied");
            }
            let events = u.recent_replies(&session, now)?;
            let mut until = now;
            if trigger != "owner" {
                if events.len() >= limits.max_replies_per_hour {
                    until =
                        until.max(events[events.len() - limits.max_replies_per_hour].at + 3600.);
                }
                let peers: Vec<_> = events
                    .iter()
                    .filter(|reply| reply.trigger == "peer")
                    .collect();
                if trigger == "peer" && peers.len() >= limits.max_echo_replies_per_hour {
                    until =
                        until.max(peers[peers.len() - limits.max_echo_replies_per_hour].at + 3600.);
                }
            }
            if until > now {
                u.defer_reply(&session, inbox, until)?;
                return Ok(Capacity::Deferred(until));
            }
            u.reserve_reply(&id, &session, inbox, &trigger, now)?;
            Ok(Capacity::Reserved)
        })
        .await
}

pub async fn claim_due(
    store: &impl Backend,
    session: String,
    now: f64,
) -> Result<Option<(i64, String)>> {
    store
        .transact(move |u| {
            // Keep the control barrier and inbox claim in the same snapshot. An
            // actor's earlier advisory check can race another actor's commit.
            if u.worker_control_pending(&session)? {
                return Ok(None);
            }
            let item = u
                .claim_next(&session, now)?
                .map(|item| (item.id, item.kind));
            Ok(item)
        })
        .await
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Answer {
    pub key: String,
    pub session: String,
    pub channel: String,
    pub thread_ts: String,
    pub text: String,
    pub obligations: Vec<String>,
    pub inbox: i64,
}
/// The caller has validated a substantive reply. A blocked notice is never an Answer.
pub async fn queue_answer(store: &impl Backend, answer: Answer, now: f64) -> Result<i64> {
    if answer.text.trim().is_empty() {
        bail!("an answer must contain text");
    }
    store
        .transact(move |u| queue_answer_tx(u, &answer, now))
        .await
}

pub(crate) fn queue_answer_tx(u: &mut dyn Unit, answer: &Answer, now: f64) -> Result<i64> {
    if answer.text.trim().is_empty() {
        bail!("an answer must contain text");
    }
    let reserved = u
        .reserved_reply(answer.inbox, &answer.session)
        .context("missing reply reservation")?;
    if let Some(id) = reserved.post {
        return Ok(id);
    }
    let route = u.thread_route(&answer.session)?;
    if (route.channel, route.root_ts) != (answer.channel.clone(), answer.thread_ts.clone()) {
        bail!("answer route differs from its thread");
    }
    let post = crate::core::delivery::Post {
        idem_key: answer.key.clone(),
        session_id: answer.session.clone(),
        kind: "reply".into(),
        channel: answer.channel.clone(),
        thread_ts: Some(answer.thread_ts.clone()),
        text: answer.text.clone(),
        meta: None,
        filename: String::new(),
        blob: None,
        after: String::new(),
    };
    let id = u.queue_post(&post, now)?;
    let queued = QueuedAnswer {
        post: id,
        session: answer.session.clone(),
        inbox: answer.inbox,
        trigger: reserved.trigger,
        obligations: answer.obligations.clone(),
        answers: serde_json::to_string(&answer.obligations)?,
        time: now,
    };
    if let Some(obligation) = u.answer_queued(&queued)? {
        bail!("obligation is not open in this thread: {obligation}");
    }
    Ok(id)
}

pub async fn delivered(store: &impl Backend, post: i64, slack_ts: String, now: f64) -> Result<()> {
    store
        .transact(move |u| u.confirm_post(post, &slack_ts, now))
        .await
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Disposition {
    Declined { reason: String },
    Deferred { until: f64, reason: String },
    Expired { reason: String },
    OwnerClosed { reason: String },
}
pub async fn disposition(
    store: &impl Backend,
    id: String,
    change: Disposition,
    actor: Authority,
    now: f64,
) -> Result<()> {
    let (state, reason, due) = match &change {
        Disposition::Declined { reason } => ("declined", reason, None),
        Disposition::Deferred { until, reason } => {
            if !until.is_finite() || *until <= now {
                bail!("deferral must be in the future");
            }
            ("deferred", reason, Some(*until))
        }
        Disposition::Expired { reason } => ("expired", reason, None),
        Disposition::OwnerClosed { reason } => {
            if actor != Authority::Owner {
                bail!("only the owner may close an obligation");
            }
            ("owner_closed", reason, None)
        }
    };
    if reason.trim().is_empty() {
        bail!("disposition requires a visible reason");
    }
    if matches!(actor, Authority::DesktopReadOnly) {
        bail!("read-only capability");
    }
    let disposal = Disposal {
        id,
        state: state.into(),
        details: serde_json::to_string(&change)?,
        due,
        actor: serde_json::to_string(&actor)?,
        time: now,
    };
    store
        .transact(move |u| {
            let current = u.obligation_state(&disposal.id)?;
            if current == "awaiting_delivery" && disposal.state != "owner_closed" {
                bail!("resolve the existing delivery before changing its obligation");
            }
            if !u.dispose(&disposal)? {
                bail!("obligation is already closed or missing");
            }
            Ok(())
        })
        .await
}

pub async fn sweep(store: &impl Backend, now: f64) -> Result<usize> {
    store.transact(move |u| u.queue_due(now)).await
}

pub async fn signal_streak(
    store: &impl Backend,
    session: String,
    streak: usize,
    threshold: usize,
    now: f64,
) -> Result<bool> {
    if threshold == 0 || streak < threshold {
        return Ok(false);
    }
    store
        .transact(move |u| u.open_signal(&format!("streak:{session}:{streak}"), &session, now))
        .await
}

pub use crate::core::worker::{retry_same_session, Failure};
