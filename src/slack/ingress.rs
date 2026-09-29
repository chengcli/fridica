//! Events API normalization. Raw envelopes belong in the private replay ledger;
//! only bounded message fields enter parent context.
use crate::attention::Message;
use serde_json::{json, Value};

pub const TEXT_LIMIT: usize = 40_000;
pub const ENVELOPE_LIMIT: usize = 4 * 1024 * 1024;

fn text(value: &Value, key: &str, limit: usize) -> String {
    value[key]
        .as_str()
        .unwrap_or("")
        .chars()
        .take(limit)
        .collect()
}
/// Slack timestamps are ASCII decimal strings; reject nonfinite/Unicode values.
pub fn timestamp(value: &str) -> bool {
    let Some((whole, fraction)) = value.split_once('.') else {
        return false;
    };
    !whole.is_empty()
        && !fraction.is_empty()
        && whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        && value.parse::<f64>().is_ok_and(f64::is_finite)
}
/// No credentials, port, redirect host or URL parser normalization can change
/// the authority that would receive a token. This is a check, not a downloader.
pub fn file_url(value: &str) -> bool {
    if value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return false;
    }
    let Some((scheme, rest)) = value.split_once("://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    scheme.eq_ignore_ascii_case("https") && host.eq_ignore_ascii_case("files.slack.com")
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
    if payload["type"] != "event_callback" {
        return None;
    }
    let event = &payload["event"];
    if event["type"] != "message"
        || !matches!(
            event["subtype"].as_str(),
            None | Some("file_share" | "thread_broadcast")
        )
        || (!event["subtype"].is_null() && !event["subtype"].is_string())
    {
        return None;
    }
    let mut fields = Vec::new();
    for value in [
        &payload["event_id"],
        &payload["team_id"],
        &event["channel"],
        &event["user"],
        &event["ts"],
    ] {
        let field = value.as_str()?;
        if field.is_empty() {
            return None;
        }
        fields.push(field.to_string());
    }
    let ts = &fields[4];
    if !timestamp(ts) {
        return None;
    }
    let thread = match &event["thread_ts"] {
        Value::Null => None,
        Value::String(t) if timestamp(t) => (t != ts).then(|| t.clone()),
        _ => return None,
    };
    let mut files = vec![];
    let mut attachments = vec![];
    for item in event["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v.is_object())
    {
        files.push(item.get("name").cloned().unwrap_or(json!("")));
        if item["id"].is_string() && item["name"].is_string() {
            let url = item["url_private"].as_str().unwrap_or("");
            attachments.push(json!({"id":text(item,"id",32),"name":text(item,"name",200),
                "mimetype":text(item,"mimetype",100),"size":item["size"].as_u64().unwrap_or(0),
                "url":if file_url(url) {url} else {""}}));
        }
    }
    let body = text(event, "text", TEXT_LIMIT);
    if body.is_empty() && files.is_empty() {
        return None;
    }
    Some(Message {
        event_id: fields[0].clone(),
        workspace: fields[1].clone(),
        channel: fields[2].clone(),
        sender: fields[3].clone(),
        ts: ts.clone(),
        thread_ts: thread,
        text: body,
        files,
        source: source.into(),
        meta: parse_metadata(&event["metadata"]),
        attachments,
    })
}
