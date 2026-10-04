//! Worker-result snapshots and provenance. The inbox notification is only a wake
//! up: durable job rows determine group readiness and whether a result was used.
use crate::core::parent::{Decision, ParentRequest};
use anyhow::Result;
use fridica_core::store::{InboxEntry, Unit};
use serde_json::{json, Value};
use std::collections::HashSet;

pub(super) fn load(u: &mut dyn Unit, session: &str, reference: &str) -> Result<Value> {
    let group = u.job_group(reference, session)?;
    let Some(group) = group else {
        return Ok(json!({"results":[],"pending":false}));
    };
    // A job stopped by a usage limit carries the limit's reset time (#107).
    let rows = u.group_results(session, &group, reference)?;
    let rows: Vec<Value> = rows
        .iter()
        .map(|s| serde_json::from_str(s))
        .collect::<std::result::Result<_, _>>()?;
    let pending = rows
        .iter()
        .any(|r| matches!(r["job_status"].as_str(), Some("queued" | "running")));
    let total = rows.len();
    let rows: Vec<_> = rows.into_iter().filter(|r| r["reported"] == 0).collect();
    let mut origin = Value::Null;
    let mut inbox = rows.first().and_then(|r| r["inbox_id"].as_i64());
    let mut visited = HashSet::new();
    while let Some(id) = inbox {
        if !visited.insert(id) {
            break;
        }
        let Some(InboxEntry {
            kind,
            reference,
            payload,
        }) = u.inbox_entry(id, session)?
        else {
            break;
        };
        let payload: Value = serde_json::from_str(&payload)?;
        if kind == "owner_instruction" || payload["owner_trigger"] == true {
            origin = json!({"class":"owner"});
            break;
        }
        if kind == "message" {
            if let Some((event, peer)) = u.message_origin(&reference)? {
                origin = json!({"event_id":event,"class":if peer {"peer"} else {"human"}});
            }
            break;
        }
        if !matches!(kind.as_str(), "worker_result" | "worker_interrupted") {
            break;
        }
        inbox = u.job_inbox(&reference, session)?;
    }
    Ok(json!({"results":rows,"pending":pending,"group_size":total,"origin":origin}))
}

/// The fast path only closes the original mention. Other asks still need an
/// explicit parent disposition. Blocked discussions must be reopened explicitly.
pub(super) fn direct(
    request: &ParentRequest,
    enabled: bool,
    limit: usize,
) -> Result<Option<Decision>> {
    if !enabled || request.session["status"] == "blocked" || request.trigger["group_size"] != 1 {
        return Ok(None);
    }
    let r = &request.trigger["results"][0];
    if r["job_status"] != "done" || r["result"]["status"] != "done" {
        return Ok(None);
    }
    let Some(report) = r["result"]["report"]
        .as_str()
        .filter(|r| !r.trim().is_empty())
    else {
        return Ok(None);
    };
    // A sign-off is the parent's own verdict (provision04): a report that
    // carries one goes to the parent instead of being posted as is.
    if report.lines().any(|line| {
        line.trim_start()
            .to_ascii_uppercase()
            .starts_with("SIGN-OFF")
    }) {
        return Ok(None);
    }
    let event = request.trigger["origin"]["event_id"].as_str();
    let answers: Vec<_> = request
        .obligations
        .iter()
        .filter(|o| {
            o["kind"] == "mention"
                && matches!(o["state"].as_str(), Some("open" | "deferred"))
                && event.is_some()
                && o["source"]["event_id"].as_str() == event
        })
        .map(|o| o["id"].clone())
        .collect();
    let (text, details) = if report.chars().count() > limit {
        let text: String = report.chars().take(limit.saturating_sub(40)).collect();
        (format!("{text}\n\nFull report attached."), report)
    } else {
        (report.to_owned(), "")
    };
    Ok(Some(serde_json::from_value(
        json!({"reply":{"text":text,"details":details,"status":"complete","answers":answers}}),
    )?))
}

pub(super) fn attachments(
    u: &mut dyn Unit,
    request: &ParentRequest,
    reply: &crate::core::parent::Reply,
    session: &str,
    inbox: i64,
    now: f64,
) -> Result<()> {
    use crate::core::delivery::Post;
    let mut uploads = Vec::new();
    if !reply.details.is_empty() {
        uploads.push((
            format!("details-{inbox}.md"),
            reply.details.as_bytes().to_vec(),
        ));
    }
    // Only a file deliverable posts files; a report's result is its text, and
    // files a worker kept along the way stay in its workspace.
    if let Some(results) = request.trigger["results"].as_array() {
        let jobs: Vec<Option<String>> = results
            .iter()
            .map(|r| r["id"].as_str().map(str::to_owned))
            .collect();
        for (path, blob) in u.deliverable_files(session, &jobs)? {
            let name = std::path::Path::new(&path)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("artifact")
                .to_owned();
            uploads.push((name, blob));
        }
    }
    for (index, (filename, blob)) in uploads.into_iter().enumerate() {
        let id = u.queue_post(
            &Post {
                idem_key: format!("{inbox}:upload:{index}"),
                session_id: session.into(),
                kind: "upload".into(),
                channel: request.session["channel"].as_str().unwrap_or("").into(),
                thread_ts: request.session["root_ts"].as_str().map(str::to_owned),
                text: String::new(),
                meta: None,
                filename,
                blob: Some(blob),
                after: format!("{inbox}:reply"),
            },
            now,
        )?;
        u.link_answers(id, &reply.answers)?;
    }
    Ok(())
}
