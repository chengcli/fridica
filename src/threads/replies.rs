//! Repeat suppression uses the delivery visible at commit, not a stale model
//! snapshot. A new question is allowed; an uncertain send is never retried merely
//! because a follow-up arrived.
use crate::core::{parent::ParentRequest, policy};
use anyhow::Result;
use fridica_core::store::{LastReply, Unit};
use serde_json::{json, Value};

pub(super) fn repeat_evidence(
    u: &mut dyn Unit,
    request: &ParentRequest,
    owner: &str,
) -> Result<Value> {
    let last = u.last_reply(request.session["id"].as_str().unwrap_or(""))?;
    let message = &request.trigger["message"];
    let text = message["text"].as_str().unwrap_or("");
    let sender = message["sender"].as_str().unwrap_or("");
    let kind = request.trigger["kind"].as_str().unwrap_or("");
    let (allowed, reason) = if matches!(
        kind,
        "owner_instruction" | "worker_result" | "worker_interrupted" | "post_refused"
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
    } else if last.as_ref().is_some_and(|r| r.state == "failed") {
        (true, "previous reply definitely failed")
    } else if !message["meta"].is_null() {
        (false, "peer follow-up")
    } else if !sender.is_empty() && last.as_ref().is_some_and(|r| sender != r.requester) {
        (true, "different requester")
    } else if text.contains('?') {
        (true, "new question")
    } else {
        (false, "same requester without a new ask")
    };
    Ok(
        json!({"allowed":allowed,"reason":reason,"previous":last.map(|LastReply{id,state,requester}|json!({"id":id,"state":state,"requester":requester}))}),
    )
}

/// Details at most this long are folded into the reply instead of attached.
const INLINE_DETAILS: usize = 1500;

pub(super) fn render(
    u: &mut dyn Unit,
    request: &ParentRequest,
    owner: &str,
    reply: &mut crate::core::parent::Reply,
    limit: usize,
) -> Result<()> {
    let session = request.session["id"].as_str().unwrap_or("");
    let messages = u.latest_messages(session)?;
    let people = crate::core::render::participants(owner, messages);
    let requester = if request.trigger["kind"] == "owner_instruction"
        || request.trigger["origin"]["class"] == "owner"
    {
        owner.to_owned()
    } else if let Some(sender) = request.trigger["message"]["sender"].as_str() {
        sender.to_owned()
    } else if let Some(event) = request.trigger["origin"]["event_id"].as_str() {
        u.message_sender(event, session)?.unwrap_or_default()
    } else {
        String::new()
    };
    // A few lines of details are not worth a file: they go in the message.
    let combined = format!("{}\n\n{}", reply.text.trim_end(), reply.details.trim());
    if !reply.details.trim().is_empty()
        && reply.details.chars().count() <= INLINE_DETAILS
        && combined.chars().count() <= limit
    {
        reply.text = combined;
        reply.details.clear();
    }
    (reply.text, reply.details) = crate::core::render::reply(
        &reply.text,
        &reply.details,
        matches!(reply.status, crate::core::parent::ReplyStatus::Waiting),
        &requester,
        &people,
        limit,
    );
    Ok(())
}
