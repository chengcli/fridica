//! Read only configured Slack channels through fixed API operations. Permalinks
//! select message IDs, never HTTP destinations. Linked text is untrusted data.
pub use fridica_slack::links::{
    permalinks, select, timestamp, Entry, Failure, Link, Reader, LINK_LIMIT, REPLY_LIMIT,
};
use serde_json::{json, Value};
use std::time::Duration;

pub const TEXT_BUDGET: usize = 20_000;

/// Trigger first, then newest history. All reads share one preparation deadline.
/// Recording faults are fatal: no model call may use an unrecorded snapshot.
pub async fn read(
    reader: &dyn Reader,
    channels: &[String],
    channel: &str,
    root: &str,
    messages: &[Value],
    timeout: Duration,
) -> Result<Vec<Value>, Failure> {
    let text = messages
        .iter()
        .filter_map(|m| m["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let deadline = tokio::time::Instant::now() + timeout;
    let mut budget = TEXT_BUDGET;
    let mut found = vec![];
    for link in permalinks(&text) {
        if budget == 0 {
            break;
        }
        let url = link.link.clone();
        if !channels.contains(&link.channel) {
            found.push(json!({"link":url,"error":"not in a channel I watch"}));
            continue;
        }
        if link.channel == channel && link.root.as_deref().unwrap_or(&link.ts) == root {
            continue;
        }
        let result = if tokio::time::Instant::now() >= deadline {
            Err(Failure::Unavailable)
        } else {
            tokio::time::timeout_at(deadline, reader.fetch(link))
                .await
                .unwrap_or(Err(Failure::Unavailable))
        };
        match result {
            Err(Failure::Recording) => return Err(Failure::Recording),
            Err(Failure::Unavailable) => {
                found.push(json!({"link":url,"error":"could not be fetched"}))
            }
            Ok(entries) if entries.is_empty() => {
                found.push(json!({"link":url,"error":"not found"}))
            }
            Ok(entries) => {
                for entry in entries.into_iter().take(REPLY_LIMIT) {
                    let text: String = entry.text.chars().take(budget).collect();
                    budget -= text.chars().count();
                    found.push(json!({"link":url,"sender":entry.sender,"text":text}));
                    if budget == 0 {
                        break;
                    }
                }
            }
        }
    }
    Ok(found)
}
