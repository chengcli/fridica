//! Durable attention operations. All decisions that reserve capacity or satisfy
//! an obligation share a transaction with the inbox/outbox effects they describe.
pub mod backfill;
use crate::{
    config::Attention,
    core::{ids::ThreadId, Authority},
    store::Store,
};
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
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
    store: &Store,
    msg: Message,
    owner: String,
    now: f64,
    grace: f64,
    obligation_id: String,
) -> Result<Option<i64>> {
    store
        .call(move |c| {
            let tx = c.transaction()?;
            let inbox = intake_tx(&tx, msg, &owner, now, grace, &obligation_id, None)?;
            tx.commit()?;
            Ok(inbox)
        })
        .await
}

/// Shared atomic intake for direct callers, Socket Mode and catch-up. A catch-up
/// cutoff decides history-only status against the thread in this transaction.
pub(crate) fn intake_tx(
    c: &rusqlite::Connection,
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
    let mentioned = msg.text.contains(&format!("<@{owner}>"));
    let mut work = true;
    let mut created = now;
    if let Some(cutoff) = history_before {
        let timestamp: f64 = msg.ts.parse()?;
        if !timestamp.is_finite() || !cutoff.is_finite() {
            bail!("invalid catch-up time");
        }
        let waiting: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id=? AND status='waiting')",
            [&session],
            |r| r.get(0),
        )?;
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
    c.execute(
        "INSERT INTO replay_events(kind,time,payload_json) VALUES('intake',?,?)",
        params![now, record.to_string()],
    )?;
    let root = msg.thread_ts.as_ref().unwrap_or(&msg.ts);
    let inserted=c.execute("INSERT OR IGNORE INTO messages(event_id,workspace,channel,ts,root_ts,thread_ts,sender,text,files_json,source,meta_json,received_at,attachments_json,mentions_owner) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![msg.event_id,msg.workspace,msg.channel,msg.ts,root,msg.thread_ts,msg.sender,msg.text,serde_json::to_string(&msg.files)?,msg.source,msg.meta.map(|m|m.to_string()),now,serde_json::to_string(&msg.attachments)?,mentioned])?;
    if inserted == 0 {
        return Ok(None);
    }
    c.execute("INSERT OR IGNORE INTO threads(id,workspace,channel,root_ts,created,updated,control_json) VALUES(?,?,?,?,?,?,'{\"kind\":\"active\"}')",
        params![session,msg.workspace,msg.channel,root,created,created])?;
    if msg.source == "self" || !work {
        return Ok(None);
    }
    c.execute(
        "INSERT INTO thread_inbox(session_id,kind,ref,created) VALUES(?,'message',?,?)",
        params![session, msg.event_id, now],
    )?;
    let inbox = c.last_insert_rowid();
    if mentioned && msg.sender != owner {
        c.execute("INSERT OR IGNORE INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated) VALUES(?,?,'mention',?,?,'Owner mentioned',?,?,?)",
            params![obligation_id,session,format!("mention:{}:{}:{}",msg.workspace,msg.channel,msg.ts),json!({"event_id":msg.event_id}).to_string(),now,now+grace,now])?;
    }
    Ok(Some(inbox))
}

pub async fn reserve(
    store: &Store,
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
    store.call(move |c| {
        let tx=c.transaction()?;
        let active:bool=tx.query_row("SELECT control='active' FROM threads WHERE id=?",[&session],|r|r.get(0))?;
        if !active {bail!("thread is paused or closed; reply reservation refused");}
        let belongs:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND session_id=? AND state IN ('pending','processing'))",params![inbox,session],|r|r.get(0))?;
        if !belongs {bail!("inbox item is not available in this session");}
        let prior:Option<String>=tx.query_row("SELECT state FROM reply_reservations WHERE inbox_id=?",[inbox],|r|r.get(0)).optional()?;
        if prior.as_deref()==Some("reserved") {return Ok(Capacity::Reserved);}
        if prior.as_deref()==Some("sent") {bail!("inbox item already replied");}
        let events:Vec<(String,f64)>=tx.prepare("SELECT r.trigger_class,CASE WHEN r.state='sent' THEN o.delivered_at ELSE MAX(r.reserved_at,?) END AS at
             FROM reply_reservations r LEFT JOIN outbox o ON o.id=r.outbox_id
             WHERE r.session_id=? AND r.state!='released' AND r.trigger_class!='owner'
             AND (r.state='reserved' OR o.delivered_at>?) ORDER BY at")?
            .query_map(params![now,session,now-3600.],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
        let mut until=now;
        if trigger!="owner" {
            if events.len()>=limits.max_replies_per_hour {
                until=until.max(events[events.len()-limits.max_replies_per_hour].1+3600.);
            }
            let peers:Vec<_>=events.iter().filter(|(class,_)|class=="peer").collect();
            if trigger=="peer" && peers.len()>=limits.max_echo_replies_per_hour {
                until=until.max(peers[peers.len()-limits.max_echo_replies_per_hour].1+3600.);
            }
        }
        if until>now {
            tx.execute("UPDATE thread_inbox SET state='pending',not_before=? WHERE id=?",params![until,inbox])?;
            tx.execute("UPDATE threads SET throttled_until=? WHERE id=?",params![until,session])?;
            tx.commit()?;return Ok(Capacity::Deferred(until));
        }
        tx.execute("INSERT INTO reply_reservations(id,session_id,inbox_id,trigger_class,reserved_at) VALUES(?,?,?,?,?)
            ON CONFLICT(inbox_id) DO UPDATE SET state='reserved',reserved_at=excluded.reserved_at,trigger_class=excluded.trigger_class",
            params![id,session,inbox,trigger,now])?;
        tx.commit()?;Ok(Capacity::Reserved)
    }).await
}

pub async fn claim_due(store: &Store, session: String, now: f64) -> Result<Option<(i64, String)>> {
    store.call(move |c| {
        let tx=c.transaction()?;
        let item:Option<(i64,String)>=tx.query_row("SELECT id,kind FROM thread_inbox WHERE session_id=? AND state='pending' AND not_before<=?
            AND NOT EXISTS(SELECT 1 FROM thread_inbox busy WHERE busy.session_id=thread_inbox.session_id AND busy.state='processing') ORDER BY id LIMIT 1",
            params![session,now],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((id,_))=&item {tx.execute("UPDATE thread_inbox SET state='processing' WHERE id=?",[id])?;}
        tx.commit()?;Ok(item)
    }).await
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
pub async fn queue_answer(store: &Store, answer: Answer, now: f64) -> Result<i64> {
    if answer.text.trim().is_empty() {
        bail!("an answer must contain text");
    }
    store
        .call(move |c| {
            let tx = c.transaction()?;
            let id = queue_answer_tx(&tx, &answer, now)?;
            tx.commit()?;
            Ok(id)
        })
        .await
}

pub(crate) fn queue_answer_tx(c: &rusqlite::Connection, answer: &Answer, now: f64) -> Result<i64> {
    if answer.text.trim().is_empty() {
        bail!("an answer must contain text");
    }
    let (trigger,prior):(String,Option<i64>)=c.query_row("SELECT trigger_class,outbox_id FROM reply_reservations WHERE inbox_id=? AND session_id=? AND state='reserved'",
        params![answer.inbox,answer.session],|r|Ok((r.get(0)?,r.get(1)?))).context("missing reply reservation")?;
    if let Some(id) = prior {
        return Ok(id);
    }
    let route: (String, String) = c.query_row(
        "SELECT channel,root_ts FROM threads WHERE id=?",
        [&answer.session],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if route != (answer.channel.clone(), answer.thread_ts.clone()) {
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
    let id = crate::store::outbox::enqueue_tx(c, &post, now)?;
    c.execute(
        "UPDATE outbox SET answers_json=?,trigger_class=? WHERE id=?",
        params![serde_json::to_string(&answer.obligations)?, trigger, id],
    )?;
    for obligation in &answer.obligations {
        let changed=c.execute("UPDATE obligations SET state='awaiting_delivery',updated=? WHERE id=? AND session_id=? AND state IN ('open','deferred')",params![now,obligation,answer.session])?;
        if changed != 1 {
            bail!("obligation is not open in this thread: {obligation}");
        }
        c.execute(
            "INSERT OR IGNORE INTO obligation_posts VALUES(?,?)",
            params![obligation, id],
        )?;
    }
    c.execute(
        "UPDATE reply_reservations SET outbox_id=? WHERE inbox_id=?",
        params![id, answer.inbox],
    )?;
    c.execute(
        "UPDATE thread_inbox SET state='done' WHERE id=?",
        [answer.inbox],
    )?;
    Ok(id)
}

pub async fn delivered(store: &Store, post: i64, slack_ts: String, now: f64) -> Result<()> {
    store
        .call(move |c| {
            let tx = c.transaction()?;
            crate::store::outbox::confirm_tx(&tx, post, &slack_ts, now)?;
            tx.commit()?;
            Ok(())
        })
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
    store: &Store,
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
    store.call(move |c| {
        let tx = c.transaction()?;
        let current: String = tx.query_row("SELECT state FROM obligations WHERE id=?", [&id], |r| r.get(0))?;
        if current == "awaiting_delivery" && state != "owner_closed" {
            bail!("resolve the existing delivery before changing its obligation");
        }
        if tx.execute("UPDATE obligations SET state=?,state_json=?,due=COALESCE(?,due),updated=? WHERE id=? AND state IN ('open','deferred','awaiting_delivery')",
            params![state,serde_json::to_string(&change)?,due,now,id])?!=1 {bail!("obligation is already closed or missing");}
        tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,'obligation.disposition',?,?)", params![now,serde_json::to_string(&actor)?,id,serde_json::to_string(&change)?])?;
        tx.commit()?;
        Ok(())
    }).await
}

pub async fn sweep(store: &Store, now: f64) -> Result<usize> {
    store.call(move |c| {
        Ok(c.execute("INSERT OR IGNORE INTO thread_inbox(session_id,kind,ref,payload_json,created,dedup_key)
            SELECT session_id,'obligation_due',id,'{}',?, 'due:'||id||':'||due FROM obligations
            WHERE state IN ('open','deferred') AND due<=?",params![now,now])?)
    }).await
}

pub async fn signal_streak(
    store: &Store,
    session: String,
    streak: usize,
    threshold: usize,
    now: f64,
) -> Result<bool> {
    if threshold == 0 || streak < threshold {
        return Ok(false);
    }
    store.call(move |c| {
        let changed=c.execute("INSERT OR IGNORE INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated)
            VALUES(?,?,'signal',?,'{}','Conversation needs attention',?,?,?)",
            params![format!("streak:{session}:{streak}"),session,format!("streak:{session}:{streak}"),now,now,now])?;
        Ok(changed==1)
    }).await
}

pub use crate::core::worker::{retry_same_session, Failure};
