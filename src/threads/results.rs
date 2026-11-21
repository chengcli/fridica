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
    let mut rows: Vec<_> = rows.into_iter().filter(|r| r["reported"] == 0).collect();
    for row in &mut rows {
        let job = u.job_record(row["id"].as_str().unwrap_or(""))?;
        if matches!(job.deliverable.as_str(), "markdown" | "figures_pdf")
            || row["result"]["artifacts"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        {
            row["deliverable"] = json!(job.deliverable);
        }
    }
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
    // A text report that also lists files needs a parent decision: forwarding
    // its prose verbatim can say "attached" for files that cannot be sent.
    if r["deliverable"] == "report" || r["job_status"] != "done" || r["result"]["status"] != "done"
    {
        return Ok(None);
    }
    let Some(report) = r["result"]["report"]
        .as_str()
        .filter(|r| !r.trim().is_empty())
    else {
        return Ok(None);
    };
    // Preserve parent escalation for legacy Slack sign-offs. GitHub review
    // reports use publication summaries instead.
    if request.session["github_reviews"] != true
        && report.lines().any(|line| {
            line.trim_start()
                .to_ascii_uppercase()
                .starts_with("SIGN-OFF")
        })
    {
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
    let rendered = if request.session["github_reviews"] == true {
        super::verdict::github_summary(report)
    } else {
        super::verdict::hoist(report)
    };
    let report: &str = &rendered;
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

/// One local attempt for an unchanged-file delivery after remote admission
/// fails. When the failed job was free-form the parent must identify the
/// intended file, especially when several versions share a filename.
pub(super) async fn recover_local(
    store: &impl fridica_core::store::Store,
    scope: fridica_core::delegation::Scope<'_>,
    request: &mut ParentRequest,
) -> Result<Option<Decision>> {
    use crate::workers::redelivery;
    let Some(results) = request.trigger["results"].as_array() else {
        return Ok(None);
    };
    let briefs: Vec<_> = results
        .iter()
        .filter(|r| {
            r["job_status"] == "failed"
                && r["deliverable"] == "markdown"
                && r["error"]
                    .as_str()
                    .and_then(redelivery::startup_error)
                    .is_some()
                && r["machine"]
                    .as_str()
                    .and_then(|name| scope.machines.get(name))
                    .is_some_and(|m| m.transport == "ssh")
        })
        .filter_map(|r| r["brief"].as_str().map(str::to_owned))
        .collect();
    if briefs.is_empty() || request.session["status"] == "blocked" {
        return Ok(None);
    }
    let Some((machine, workspace)) = scope
        .machines
        .machines
        .iter()
        .filter(|m| m.transport == "local")
        .find_map(|m| m.workspaces.first().map(|w| (m, w)))
    else {
        return Ok(None);
    };
    let session = request.session["id"].as_str().unwrap_or("").to_owned();
    let requested = (results.len() == 1)
        .then(|| redelivery::requested(&briefs[0]))
        .flatten()
        .map(|(name, digest)| (name.to_owned(), digest.to_ascii_lowercase()));
    let (verified, files) = store
        .transact(move |u| {
            if let Some((name, digest)) = requested {
                if redelivery::saved(u, &session, &name, &digest)?.is_some() {
                    return Ok((Some((name, digest)), Vec::new()));
                }
            }
            Ok((None, redelivery::candidates(u, &session, &briefs)?))
        })
        .await?;
    if let Some((name, digest)) = verified {
        let decision = serde_json::from_value(json!({
            "delegations":[{"brief":format!("Resend saved file {digest}\n{name}"),"machine":machine.name,
                "workspace":workspace.name,"deliverable":"markdown","ephemeral":true,"context":"fresh"}],
            "reply":{"text":"A verified copy is available. I queued a local worker to return it.","status":"complete"}
        }))?;
        if fridica_core::delegation::prepare(&decision, request, Some(scope), None).is_ok() {
            return Ok(Some(decision));
        }
    }
    if !files.is_empty() {
        request.trigger["local_file_recovery"] =
            json!({"machine":machine.name,"workspace":workspace.name,"files":files});
    }
    Ok(None)
}

/// Derive the upload list and diagnostics from durable worker results. Read
/// them in the same transaction that queues uploads and marks results used.
#[derive(Default)]
pub(super) struct Files {
    uploads: Vec<(String, Vec<u8>)>,
    notices: Vec<String>,
}

fn file_name(path: &str) -> &str {
    std::path::Path::new(path)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("artifact")
}

impl Files {
    pub fn load(u: &mut dyn Unit, request: &ParentRequest, session: &str) -> Result<Self> {
        let mut files = Self::default();
        for r in request.trigger["results"].as_array().into_iter().flatten() {
            let kind = r["deliverable"].as_str().unwrap_or("");
            let refs = r["result"]["artifacts"].as_array().into_iter().flatten();
            if kind == "report" {
                let names: Vec<_> = refs
                    .filter_map(|f| f["path"].as_str())
                    .map(file_name)
                    .collect();
                files.notices.push(format!("This was a text-report job. No upload was queued for {}. A file deliverable is needed to post it.", names.join(", ")));
                continue;
            }
            if !matches!(kind, "markdown" | "figures_pdf") {
                continue;
            }
            let paths = u.deliverable_files(session, &[r["id"].as_str().map(str::to_owned)])?;
            let missing: Vec<_> = refs
                .filter_map(|f| f["path"].as_str())
                .filter(|p| !paths.iter().any(|(ready, _)| ready == p))
                .map(file_name)
                .collect();
            if !missing.is_empty() {
                files.notices.push(format!(
                    "Could not collect {} for upload.",
                    missing.join(", ")
                ));
            } else if paths.is_empty() {
                let code = r["error"].as_str().unwrap_or("");
                let reason = crate::workers::redelivery::startup_error(code)
                    .unwrap_or(match code {
                    "saved_file_not_verified" => {
                        "No saved file in this thread matches the requested filename and SHA-256."
                    }
                    "saved_file_read_failed" => "Fridica could not read the saved file.",
                    "" => "The worker returned no file.",
                    _ => "The worker did not produce a deliverable.",
                });
                let code = if code.is_empty() {
                    String::new()
                } else {
                    format!(" ({code})")
                };
                files.notices.push(format!(
                    "{reason}{code} No file from this job was queued for upload."
                ));
            }
            files.uploads.extend(
                paths
                    .into_iter()
                    .map(|(path, bytes)| (file_name(&path).to_owned(), bytes)),
            );
        }
        Ok(files)
    }

    pub fn describe(&self, reply: &mut crate::core::parent::Reply) {
        let mut lines = self.notices.clone();
        if !self.uploads.is_empty() {
            let names: Vec<_> = self.uploads.iter().map(|(name, _)| name.as_str()).collect();
            lines.push(format!("Files queued for upload: {}.", names.join(", ")));
        }
        if !lines.is_empty() {
            // Preserve a checksum or verdict on the first line, and a verdict
            // on the last line of a longer report.
            let text = reply.text.trim_start();
            let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
            let status = lines.join("\n");
            reply.text = [first, &status, rest]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
        }
    }

    pub fn queue(
        mut self,
        u: &mut dyn Unit,
        request: &ParentRequest,
        reply: Option<&crate::core::parent::Reply>,
        session: &str,
        inbox: i64,
        now: f64,
    ) -> Result<()> {
        use crate::core::delivery::Post;
        if let Some(reply) = reply.filter(|r| !r.details.is_empty()) {
            self.uploads.insert(
                0,
                (
                    format!("details-{inbox}.md"),
                    reply.details.as_bytes().to_vec(),
                ),
            );
        }
        let channel = request.session["channel"].as_str().unwrap_or("");
        let root_ts = request.session["root_ts"].as_str().map(str::to_owned);
        if reply.is_none() && !self.notices.is_empty() {
            u.queue_post(
                &Post {
                    text: self.notices.join("\n"),
                    ..super::posts::post(
                        format!("{inbox}:file-notice"),
                        session,
                        "notice",
                        channel,
                        root_ts.clone(),
                    )
                },
                now,
            )?;
        }
        let after = reply.map_or_else(String::new, |_| format!("{inbox}:reply"));
        for (index, (filename, blob)) in self.uploads.into_iter().enumerate() {
            let id = u.queue_post(
                &Post {
                    filename,
                    blob: Some(blob),
                    after: after.clone(),
                    ..super::posts::post(
                        format!("{inbox}:upload:{index}"),
                        session,
                        "upload",
                        channel,
                        root_ts.clone(),
                    )
                },
                now,
            )?;
            if let Some(reply) = reply {
                u.link_answers(id, &reply.answers)?;
            }
        }
        Ok(())
    }
}
