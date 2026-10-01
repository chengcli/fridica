//! Events API normalization from fridica-slack, with Fridica's own message
//! metadata parsed. Raw envelopes belong in the private replay ledger; only
//! bounded message fields enter parent context.
use crate::attention::Message;
pub use fridica_slack::ingress::{file_url, timestamp, ENVELOPE_LIMIT, TEXT_LIMIT};
use serde_json::{json, Value};

fn text(value: &Value, key: &str, limit: usize) -> String {
    value[key]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(limit)
        .collect()
}
pub fn parse_metadata(value: &Value) -> Option<Value> {
    if value["event_type"] != "fridica_message" {
        return None;
    }
    let data = &value["event_payload"];
    let session = text(data, "session", 128);
    Some(json!({
        "owner": text(data,"owner",32),
        "session": if session.is_empty() { text(data,"task_id",128) } else { session },
        "turn": data["turn"].as_u64().unwrap_or(0).min(10_000),
        "status": data["status"].as_str().filter(|s|matches!(*s,"complete"|"waiting"|"blocked")).unwrap_or(""),
        "kind": ({ let k=text(data,"kind",32); if k.is_empty() { "reply".to_string() } else { k } }),
        "worker": text(data,"worker",64),
        "v": match data["v"].as_u64() { Some(2)=>2, _=>1 }
    }))
}
pub fn normalize(payload: &Value, source: &str) -> Option<Message> {
    let m = fridica_slack::ingress::normalize(payload, source)?;
    Some(Message {
        event_id: m.event_id,
        workspace: m.workspace,
        channel: m.channel,
        sender: m.sender,
        ts: m.ts,
        thread_ts: m.thread_ts,
        text: m.text,
        files: m.files,
        source: m.source,
        meta: parse_metadata(&m.metadata),
        attachments: m.attachments,
    })
}
