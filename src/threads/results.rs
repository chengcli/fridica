//! Worker-result snapshots and provenance. The inbox notification is only a wake
//! up: durable job rows determine group readiness and whether a result was used.
use crate::core::parent::{Decision, ParentRequest};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;

pub(super) fn load(c: &Connection, session: &str, reference: &str) -> Result<Value> {
    let group: Option<String> = c
        .query_row(
            "SELECT join_group FROM jobs WHERE id=? AND session_id=?",
            [reference, session],
            |r| r.get(0),
        )
        .optional()?;
    let Some(group) = group else {
        return Ok(json!({"results":[],"pending":false}));
    };
    let rows: Vec<String> = c.prepare("SELECT json_object('id',j.id,'worker_id',j.worker_id,'machine',w.machine,'workspace',w.workspace,'role',w.role,
        'brief',j.brief,'job_status',j.status,'error',j.error,'result',json(j.result_json),'inbox_id',j.inbox_id,'reported',j.reported,'attempt',j.attempt)
        FROM jobs j JOIN workers w ON w.id=j.worker_id WHERE j.session_id=? AND ((?!='' AND j.join_group=?) OR (?='' AND j.id=?)) ORDER BY j.queued_at,j.rowid")?
        .query_map([session,&group,&group,&group,reference], |r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
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
        let item: Option<(String, String, String)> = c
            .query_row(
                "SELECT kind,ref,payload_json FROM thread_inbox WHERE id=? AND session_id=?",
                rusqlite::params![id, session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((kind, reference, payload)) = item else {
            break;
        };
        let payload: Value = serde_json::from_str(&payload)?;
        if kind == "owner_instruction" || payload["owner_trigger"] == true {
            origin = json!({"class":"owner"});
            break;
        }
        if kind == "message" {
            let data: Option<(String, bool)> = c
                .query_row(
                    "SELECT event_id,meta_json IS NOT NULL FROM messages WHERE event_id=?",
                    [&reference],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((event, peer)) = data {
                origin = json!({"event_id":event,"class":if peer {"peer"} else {"human"}});
            }
            break;
        }
        if !matches!(kind.as_str(), "worker_result" | "worker_interrupted") {
            break;
        }
        inbox = c
            .query_row(
                "SELECT inbox_id FROM jobs WHERE id=? AND session_id=?",
                [reference, session.into()],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
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
    c: &Connection,
    request: &ParentRequest,
    reply: &crate::core::parent::Reply,
    session: &str,
    inbox: i64,
    now: f64,
) -> Result<()> {
    use crate::{core::delivery::Post, store::outbox};
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
        for r in results {
            let artifacts: Vec<(String,Vec<u8>)> = c.prepare("SELECT a.path,a.blob FROM artifacts a JOIN jobs j ON j.id=a.job_id WHERE a.job_id=? AND a.session_id=? AND a.status='ready' AND a.blob IS NOT NULL AND j.deliverable IN ('markdown','figures_pdf') ORDER BY a.id")?
                .query_map(rusqlite::params![r["id"].as_str(),session], |r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
            for (path, blob) in artifacts {
                let name = std::path::Path::new(&path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("artifact")
                    .to_owned();
                uploads.push((name, blob));
            }
        }
    }
    for (index, (filename, blob)) in uploads.into_iter().enumerate() {
        let id = outbox::enqueue_tx(
            c,
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
        for obligation in &reply.answers {
            c.execute(
                "INSERT INTO obligation_posts VALUES(?,?)",
                rusqlite::params![obligation, id],
            )?;
        }
    }
    Ok(())
}
