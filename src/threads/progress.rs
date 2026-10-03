//! Interim progress notes (#105): a note a running worker wrote goes to its
//! thread as a notice, without a parent turn or a reply reservation. The
//! outbox's egress checks and per-thread ordering apply as to any post, so
//! notes precede the job's final report.
use super::actor::{Actor, Step};
use crate::{
    core::{delivery::Post, parent::Parent, parent::ParentRequest},
    store::outbox,
};
use anyhow::{Context, Result};
use rusqlite::{params, OptionalExtension};
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
    actor.store.call(move |c| {
        let tx = c.transaction()?;
        let (processing, active): (bool, bool) = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing'),control='active' FROM threads WHERE id=?",
            params![id, session], |r| Ok((r.get(0)?, r.get(1)?)))?;
        if !processing {
            tx.commit()?;
            return Ok(Step::Stale);
        }
        // Only a note of the attempt still running is news; once the job ended
        // its result is reported instead.
        let note: Option<(String, String)> = if active {
            tx.query_row(
                "SELECT p.text,j.worker_id FROM job_progress p JOIN jobs j ON j.id=p.job_id AND j.attempt=p.attempt
                 WHERE p.job_id=? AND p.attempt=? AND p.seq=? AND j.session_id=? AND j.status='running'",
                params![job, attempt, seq, session], |r| Ok((r.get(0)?, r.get(1)?))).optional()?
        } else {
            None
        };
        let step = if let Some((text, worker)) = note {
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
            outbox::enqueue_tx(&tx, &post, now)?;
            Step::Committed
        } else {
            Step::Observed
        };
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?", [id])?;
        tx.commit()?;
        Ok(step)
    }).await
}
