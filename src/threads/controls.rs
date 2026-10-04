//! Trusted callers supply authority derived from their authenticated capability,
//! never from a request body's `actor` field. Effects and audit commit together.
use crate::{
    core::{Authority, ThreadControl},
    store::{Sqlite, Store},
};
use anyhow::{bail, Result};
use fridica_core::store::{ControlState, Ledger, ResumePoint, ThreadControls, WorkerStops};
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
            let existing = Sqlite(&tx).owner_instruction(&session, &client_id)?;
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
    let mut u = Sqlite(tx);
    let ControlState { control, details } = u.control_state(&session)?;
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
            let stopping = u.worker_stop_pending(&session)?;
            // Include older terminal threads which predate durable stop intents.
            let live = u.has_live_workers(&session)?;
            if stopping
                || crate::store::worker_controls::pending_tx(u.0, &session)?
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
    u.set_control(
        &session,
        state,
        &serde_json::to_string(&detail)?,
        reason,
        now,
        reset_streak,
    )?;
    if matches!(action, Control::Resume) {
        u.restart_turns(&session)?;
    } else if matches!(action, Control::Instruct { .. }) {
        u.unblock(&session)?;
    }
    if state == "active" {
        u.release_worker_results(&session)?;
    }
    if matches!(action, Control::Close | Control::Archive | Control::Clean) {
        // Persist termination intent with the control effect, before
        // acknowledgement. Complete only after the supervisor confirms process
        // cleanup.
        u.queue_worker_stops(std::slice::from_ref(&session), now, true)?;
    }
    if matches!(action, Control::Clean) {
        // Retain IDs and history, but never execute work using wiped input or a
        // context snapshot captured before cleaning. New intake remains visible.
        u.wipe(&session, now)?;
    }
    let mut instruction = None;
    match &action {
        Control::Instruct { text } => {
            if text.trim().is_empty() {
                bail!("instruction must contain text");
            }
            instruction = Some(u.queue_owner_instruction(
                &session,
                &client_id.unwrap_or_default(),
                &json!({"text":text}).to_string(),
                now,
            )?);
        }
        Control::Resume => {
            let ResumePoint { latest, newest } = u.resume_point(&session)?;
            let boundary = latest.as_ref().map_or(newest, |(_, ts)| ts - 1e-6);
            u.reset_thread_at(&session, boundary)?;
            if let Some((event, _)) = latest {
                // A delegated request already has durable work or a result to
                // consume; resume_message leaves it alone.
                let payload =
                    json!({"resumed":true,"owner_trigger":actor==Authority::Owner}).to_string();
                u.resume_message(&session, &event, &payload, now)?;
            }
        }
        _ => {}
    }
    u.audit_control(
        now,
        &serde_json::to_string(&actor)?,
        &format!("thread.{name}"),
        &session,
        &serde_json::to_string(&action)?,
    )?;
    u.record(
        "thread_control",
        now,
        &json!({"session":session,"authority":actor,"control":action}).to_string(),
        true,
    )?;
    Ok(instruction)
}
