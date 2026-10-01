//! Trusted callers supply authority derived from their authenticated capability,
//! never from a request body's `actor` field. Effects and audit commit together.
use crate::{
    core::{Authority, ThreadControl},
    store::Store,
};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    Pause { reason: String },
    Resume,
    Close,
    Archive,
    Restore,
    Clean,
    Instruct { text: String },
}

pub async fn apply(
    store: &Store,
    session: String,
    action: Control,
    actor: Authority,
    now: f64,
) -> Result<()> {
    if !now.is_finite() {
        bail!("invalid control time");
    }
    if actor != Authority::Owner {
        bail!("thread control requires an authenticated capability");
    }
    store
        .call(move |c| {
            let tx = c.transaction()?;
            apply_tx(&tx, session, action, actor, now, None)?;
            tx.commit()?;
            Ok(())
        })
        .await
}

/// A retry with the same client ID returns the original inbox ID without
/// reopening a subsequently paused thread or executing the instruction twice.
pub async fn instruct(
    store: &Store,
    session: String,
    text: String,
    client_id: String,
    actor: Authority,
    now: f64,
) -> Result<i64> {
    if actor != Authority::Owner || !now.is_finite() {
        bail!("owner instruction authentication required");
    }
    let text = text.trim().to_owned();
    if !(1..=4000).contains(&text.chars().count())
        || !(8..=80).contains(&client_id.len())
        || !client_id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
    {
        bail!("invalid instruction");
    }
    store
        .call(move |c| {
            let tx = c.transaction()?;
            let existing: Option<(i64, String)> = tx
                .query_row(
                    "SELECT id,json_extract(payload_json,'$.text') FROM thread_inbox \
             WHERE session_id=? AND kind='owner_instruction' AND ref=? ORDER BY id LIMIT 1",
                    params![session, client_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((id, prior)) = existing {
                if prior != text {
                    bail!("instruction ID reused with different text");
                }
                return Ok(id);
            }
            let id = apply_tx(
                &tx,
                session,
                Control::Instruct { text },
                actor,
                now,
                Some(client_id),
            )?
            .expect("instruction ID");
            tx.commit()?;
            Ok(id)
        })
        .await
}

fn apply_tx(
    tx: &rusqlite::Transaction<'_>,
    session: String,
    action: Control,
    actor: Authority,
    now: f64,
    client_id: Option<String>,
) -> Result<Option<i64>> {
    let (control, details): (String, String) = tx.query_row(
        "SELECT control,control_json FROM threads WHERE id=?",
        [&session],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut prior: ThreadControl = serde_json::from_str(&details).unwrap_or(ThreadControl::Active);
    if control == "paused" && !matches!(prior, ThreadControl::Paused { .. }) {
        prior = ThreadControl::Paused {
            by: Authority::Owner,
            reason: "Unclassified legacy pause".into(),
            since: now,
        };
    }
    let (state, name, detail) = match &action {
        Control::Pause { reason } => {
            if matches!(control.as_str(), "closed" | "archived" | "cleaned") {
                bail!("closed threads require explicit restoration");
            }
            if reason.trim().is_empty() {
                bail!("pause requires a reason");
            }
            // No other actor can relabel an owner's pause and then resume it.
            if matches!(
                prior,
                ThreadControl::Paused {
                    by: Authority::Owner,
                    ..
                }
            ) && actor != Authority::Owner
            {
                bail!("owner pause is protected");
            }
            (
                "paused",
                "pause",
                ThreadControl::Paused {
                    by: actor.clone(),
                    reason: reason.clone(),
                    since: now,
                },
            )
        }
        Control::Resume | Control::Instruct { .. } => {
            if control == "paused" && !prior.can_resume(&actor) {
                bail!("only the owner may resume an owner-paused thread");
            }
            if matches!(action, Control::Instruct { .. }) && actor != Authority::Owner {
                bail!("owner instructions require owner authentication");
            }
            if matches!(control.as_str(), "closed" | "archived" | "cleaned") {
                bail!("closed threads require explicit restoration");
            }
            (
                "active",
                if matches!(action, Control::Resume) {
                    "resume"
                } else {
                    "instruct"
                },
                ThreadControl::Active,
            )
        }
        Control::Restore => {
            if actor != Authority::Owner {
                bail!("restoration requires owner authentication");
            }
            let stopping: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM replay_events WHERE kind='thread_worker_stop' AND complete=0 AND json_extract(payload_json,'$.session')=?)",
                [&session], |r| r.get(0),
            )?;
            // Include older terminal threads which predate durable stop intents.
            let live: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM workers WHERE session_id=? AND status!='stopped')",
                [&session],
                |r| r.get(0),
            )?;
            if stopping
                || crate::store::worker_controls::pending_tx(tx, &session)?
                || (matches!(control.as_str(), "closed" | "archived" | "cleaned") && live)
            {
                bail!("thread worker cleanup is pending");
            }
            ("active", "restore", ThreadControl::Active)
        }
        Control::Close | Control::Archive | Control::Clean => {
            if actor != Authority::Owner {
                bail!("only the owner may close, archive or clean a thread");
            }
            if matches!(action, Control::Close) {
                ("closed", "close", ThreadControl::Closed)
            } else if matches!(action, Control::Archive) {
                ("archived", "archive", ThreadControl::Archived)
            } else {
                ("cleaned", "clean", ThreadControl::Cleaned)
            }
        }
    };
    let reason = match &detail {
        ThreadControl::Paused { reason, .. } => reason.as_str(),
        _ => "",
    };
    let reset_streak = state == "active" && !matches!(action, Control::Restore);
    tx.execute(
        "UPDATE threads SET control=?,control_json=?,pause_reason=?,version=version+1,updated=?,
            wait_streak=CASE WHEN ? THEN 0 ELSE wait_streak END,
            no_progress=CASE WHEN ? THEN 0 ELSE no_progress END WHERE id=?",
        params![
            state,
            serde_json::to_string(&detail)?,
            reason,
            now,
            reset_streak,
            reset_streak,
            session
        ],
    )?;
    if matches!(action, Control::Resume) {
        tx.execute("UPDATE threads SET status=CASE WHEN status IN ('blocked','working') THEN 'complete' ELSE status END,turns=0,debriefed_turn=0 WHERE id=?",[&session])?;
    } else if matches!(action, Control::Instruct { .. }) {
        tx.execute(
            "UPDATE threads SET status='complete' WHERE id=? AND status='blocked'",
            [&session],
        )?;
    }
    if state == "active" {
        tx.execute("UPDATE thread_inbox SET not_before=0 WHERE session_id=? AND kind IN ('worker_result','worker_interrupted') AND state='pending'",[&session])?;
    }
    if matches!(action, Control::Close | Control::Archive | Control::Clean) {
        queue_stops_tx(tx, &session, now, true)?;
    }
    if matches!(action, Control::Clean) {
        tx.execute("UPDATE messages SET text='',files_json='[]',attachments_json='[]' WHERE workspace||':'||channel||':'||root_ts=?", [&session])?;
        tx.execute(
            "UPDATE threads SET summary='',decisions_json='[]' WHERE id=?",
            [&session],
        )?;
        tx.execute("UPDATE thread_inbox SET payload_json='{\"text\":\"\"}' WHERE session_id=? AND kind='owner_instruction'", [&session])?;
        // Retain IDs and history, but never execute work using wiped input or a
        // context snapshot captured before cleaning. New intake remains visible.
        tx.execute("UPDATE thread_inbox SET state='dropped' WHERE session_id=? AND state IN ('pending','processing')", [&session])?;
        tx.execute("UPDATE reply_reservations SET state='released' WHERE session_id=? AND state='reserved' AND outbox_id IS NULL", [&session])?;
        tx.execute("UPDATE obligations SET state='owner_closed',state_json='{\"kind\":\"owner_closed\",\"reason\":\"Thread cleaned by owner\"}',updated=? WHERE session_id=? AND state IN ('open','deferred','awaiting_delivery')", params![now,session])?;
    }
    let mut instruction = None;
    match &action {
        Control::Instruct { text } => {
            if text.trim().is_empty() {
                bail!("instruction must contain text");
            }
            tx.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,created) VALUES(?,'owner_instruction',?,?,?)",params![session,client_id.unwrap_or_default(),json!({"text":text}).to_string(),now])?;
            instruction = Some(tx.last_insert_rowid());
        }
        Control::Resume => {
            let latest:Option<(String,f64)>=tx.query_row("SELECT event_id,CAST(ts AS REAL) FROM messages WHERE workspace||':'||channel||':'||root_ts=?
                    AND source!='self' AND meta_json IS NULL AND CAST(ts AS REAL)>COALESCE((SELECT MAX(CAST(ts AS REAL)) FROM messages WHERE workspace||':'||channel||':'||root_ts=? AND source='self'),0)
                    ORDER BY CAST(ts AS REAL) DESC LIMIT 1",params![session,session],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            let newest:f64=tx.query_row("SELECT COALESCE(MAX(CAST(ts AS REAL)),0) FROM messages WHERE workspace||':'||channel||':'||root_ts=?",[&session],|r|r.get(0))?;
            let boundary = latest.as_ref().map_or(newest, |(_, ts)| ts - 1e-6);
            tx.execute(
                "UPDATE threads SET reset_at=? WHERE id=?",
                params![boundary, session],
            )?;
            if let Some((event, _)) = latest {
                // A delegated request already has durable work or a result to
                // consume. Resume it without delegating the same ask again.
                let delegated:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs j JOIN thread_inbox i ON i.id=j.inbox_id WHERE j.session_id=? AND i.kind='message' AND i.ref=? AND (j.reported=0 OR j.status IN ('queued','running')))",params![session,event],|r|r.get(0))?;
                if !delegated {
                    let payload =
                        json!({"resumed":true,"owner_trigger":actor==Authority::Owner}).to_string();
                    let existing:Option<i64>=tx.query_row("SELECT id FROM thread_inbox WHERE session_id=? AND kind='message' AND ref=? AND state IN ('pending','processing') ORDER BY id LIMIT 1",params![session,event],|r|r.get(0)).optional()?;
                    if let Some(id) = existing {
                        tx.execute(
                            "UPDATE thread_inbox SET payload_json=?,not_before=0 WHERE id=?",
                            params![payload, id],
                        )?;
                        tx.execute("UPDATE thread_inbox SET state='done' WHERE session_id=? AND kind='message' AND ref=? AND state='pending' AND id!=?",params![session,event,id])?;
                    } else {
                        tx.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,created) VALUES(?,'message',?,?,?)",params![session,event,payload,now])?;
                    }
                }
            }
        }
        _ => {}
    }
    tx.execute(
        "INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,?,?,?)",
        params![
            now,
            serde_json::to_string(&actor)?,
            format!("thread.{name}"),
            session,
            serde_json::to_string(&action)?
        ],
    )?;
    tx.execute(
        "INSERT INTO replay_events(kind,time,payload_json) VALUES('thread_control',?,?)",
        params![
            now,
            json!({"session":session,"authority":actor,"control":action}).to_string()
        ],
    )?;
    Ok(instruction)
}

/// Persist termination intent with the control effect, before acknowledgement.
/// Complete only after the supervisor confirms process cleanup. Replay storage
/// keeps the intent recoverable even if worker status already became `stopped`.
pub(crate) fn queue_stops_tx(
    c: &rusqlite::Connection,
    session: &str,
    now: f64,
    include_stopped: bool,
) -> Result<()> {
    c.execute(
        "INSERT INTO replay_events(kind,time,payload_json,complete)
         SELECT 'thread_worker_stop',?,json_object('session',session_id,'worker',id),0 FROM workers w
         WHERE session_id=? AND (status!='stopped' OR ?) AND NOT EXISTS(
            SELECT 1 FROM replay_events e WHERE e.kind='thread_worker_stop' AND e.complete=0
            AND json_extract(e.payload_json,'$.worker')=w.id)",
        params![now,session,include_stopped],
    )?;
    Ok(())
}
