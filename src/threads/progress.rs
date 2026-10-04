//! Interim progress notes (#105): a note a running worker wrote goes to its
//! thread as a notice, without a parent turn or a reply reservation. The
//! outbox's egress checks and per-thread ordering apply as to any post, so
//! notes precede the job's final report.
use super::actor::{Actor, Step};
use crate::core::{delivery::Post, parent::Parent, parent::ParentRequest};
use anyhow::{Context, Result};
use fridica_core::store::{ProgressNote, ProgressState, Store as _};
use serde_json::json;

pub(super) async fn handle<P: Parent>(actor: &Actor<P>, request: ParentRequest) -> Result<Step> {
    let id = request.inbox_id;
    let session = request.session["id"]
        .as_str()
        .context("missing progress session")?
        .to_owned();
    let channel = request.session["channel"]
        .as_str()
        .context("missing channel")?
        .to_owned();
    let root = request.session["root_ts"]
        .as_str()
        .context("missing root timestamp")?
        .to_owned();
    let job = request.trigger["ref"].as_str().unwrap_or("").to_owned();
    let attempt = request.trigger["payload"]["attempt"].as_i64().unwrap_or(0);
    let seq = request.trigger["payload"]["seq"].as_i64().unwrap_or(0);
    let owner = actor.owner.clone();
    let now = actor.clock.now();
    actor.store.transact(move |u| {
        let ProgressState { processing, active } = u.progress_state(id, &session)?;
        if !processing {
            return Ok(Step::Stale);
        }
        // Only a note of the attempt still running is news; once the job ended
        // its result is reported instead.
        let note = if active {
            u.running_progress_note(&job, attempt, seq, &session)?
        } else {
            None
        };
        let step = if let Some(ProgressNote { text, worker }) = note {
            let post = Post {
                idem_key: format!("{id}:progress"),
                session_id: session.clone(),
                kind: "notice".into(),
                channel,
                thread_ts: Some(root),
                text,
                meta: Some(json!({"owner":owner,"session":session,"kind":"progress","worker":worker,"job":job,"v":2})),
                filename: String::new(),
                blob: None,
                after: String::new(),
            };
            u.queue_post(&post, now)?;
            Step::Committed
        } else {
            Step::Observed
        };
        u.inbox_done(id)?;
        Ok(step)
    }).await
}
