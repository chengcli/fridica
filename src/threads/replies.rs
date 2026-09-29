//! Repeat suppression uses the delivery visible at commit, not a stale model
//! snapshot. A new question is allowed; an uncertain send is never retried merely
//! because a follow-up arrived.
use crate::core::{parent::ParentRequest, policy};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

pub(super) fn repeat_evidence(
    c: &Connection,
    request: &ParentRequest,
    owner: &str,
) -> Result<Value> {
    let last: Option<(i64, String, String)> = c.query_row(
        "SELECT o.id,o.state,COALESCE(m.sender,'') FROM outbox o
         LEFT JOIN thread_inbox i ON i.session_id=o.session_id AND o.idem_key=CAST(i.id AS TEXT)||':reply' AND i.kind='message'
         LEFT JOIN messages m ON m.event_id=CASE WHEN o.trigger_event!='' THEN o.trigger_event ELSE i.ref END
         WHERE o.session_id=? AND o.kind='reply' ORDER BY o.id DESC LIMIT 1",
        [request.session["id"].as_str().unwrap_or("")],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)),
    ).optional()?;
    let message = &request.trigger["message"];
    let text = message["text"].as_str().unwrap_or("");
    let sender = message["sender"].as_str().unwrap_or("");
    let kind = request.trigger["kind"].as_str().unwrap_or("");
    let (allowed, reason) = if matches!(
        kind,
        "owner_instruction" | "worker_result" | "worker_interrupted"
    ) {
        (true, "owner instruction or worker result")
    } else if kind != "message" {
        (false, "automatic turn")
    } else if request.trigger["payload"]["resumed"] == true {
        (true, "resumed by control")
    } else if policy::repost_requested(text, owner) {
        (true, "addressed or explicit repost")
    } else if last.is_none() {
        (true, "no previous reply")
    } else if last.as_ref().is_some_and(|(_, state, _)| state == "failed") {
        (true, "previous reply definitely failed")
    } else if !message["meta"].is_null() {
        (false, "peer follow-up")
    } else if !sender.is_empty()
        && last
            .as_ref()
            .is_some_and(|(_, _, answered)| sender != answered)
    {
        (true, "different requester")
    } else if text.contains('?') {
        (true, "new question")
    } else {
        (false, "same requester without a new ask")
    };
    Ok(
        json!({"allowed":allowed,"reason":reason,"previous":last.map(|(id,state,sender)|json!({"id":id,"state":state,"requester":sender}))}),
    )
}
