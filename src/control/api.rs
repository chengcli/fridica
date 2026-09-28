//! Route compatibility over the runtime's durable operations. No direct backend
//! execution or authentication decisions are delegated to request bodies.
use super::{views, Backend, Request, Response};
use crate::{
    core::{
        delivery::{AdapterFuture, Delivery},
        parent::Parent,
        worker::ApprovalDecision,
        Authority,
    },
    store::outbox,
    threads::{controls::Control, runtime::Runtime},
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{collections::BTreeMap, sync::Arc};
pub struct Api<P: Parent, D: Delivery> {
    runtime: Arc<Runtime<P, D>>,
}
impl<P: Parent + 'static, D: Delivery + 'static> Api<P, D> {
    pub fn new(runtime: Arc<Runtime<P, D>>) -> Self {
        Self { runtime }
    }
    async fn handle(&self, request: Request, authority: Authority) -> Response {
        if !matches!(
            authority,
            Authority::Owner | Authority::Overseer | Authority::DesktopReadOnly
        ) {
            return Response::error(403, "forbidden");
        }
        let Some((parts, query)) = target(&request.target) else {
            return Response::error(400, "invalid_target");
        };
        let store = self.runtime.store();
        let now = self.runtime.now();
        if !now.is_finite() {
            return Response::error(500, "invalid_control_clock");
        }
        let record = json!({"method":request.method,"target":request.target,"body":request.body,"authority":authority});
        let call = match store.call(move |c| {
            c.execute(
                "INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('control_request',?,?,0)",
                params![now,record.to_string()],
            )?;
            Ok(c.last_insert_rowid())
        }).await {
            Ok(id) => id,
            Err(_) => return Response::error(500, "control_recording_failed"),
        };
        let response = self.route(request, authority, &parts, query).await;
        let now = self.runtime.now();
        let record = json!({"call":call,"status":response.status,"body":response.body});
        let recorded = store
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute(
                "INSERT INTO replay_events(kind,time,payload_json) VALUES('control_response',?,?)",
                params![now,record.to_string()],
            )?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?", [call])?;
                tx.commit()?;
                Ok(())
            })
            .await;
        if recorded.is_err() {
            return Response::error(500, "control_outcome_unrecorded");
        }
        response
    }
    async fn route(
        &self,
        request: Request,
        authority: Authority,
        parts: &[String],
        query: BTreeMap<String, String>,
    ) -> Response {
        let store = self.runtime.store();
        let config = self.runtime.config();
        let now = self.runtime.now();
        if request.method == "GET" {
            if !request.body.as_object().is_some_and(|v| v.is_empty()) {
                return Response::error(400, "get_body_not_allowed");
            }
            let processes = if matches!(
                parts.first().map(String::as_str),
                Some("workers" | "machines")
            ) || (parts.first().is_some_and(|p| p == "threads")
                && parts.len() == 2)
            {
                self.runtime.processes().await
            } else {
                BTreeMap::new()
            };
            let parts = parts.to_vec();
            let observe_only = self.runtime.observe_only();
            return match store
                .call(move |c| {
                    let tx = c.transaction()?;
                    let result =
                        views::get(&tx, &config, &parts, &query, &processes, observe_only)?;
                    tx.commit()?;
                    Ok(result)
                })
                .await
            {
                Ok(Some(value)) => Response::ok(value),
                Ok(None) => Response::error(404, "not_found"),
                Err(_) => Response::error(500, "view_unavailable"),
            };
        }
        if authority == Authority::DesktopReadOnly {
            return Response::error(403, "read_only_capability");
        }
        if !request.body.is_object() || !query.is_empty() {
            return Response::error(400, "invalid_body_or_query");
        }
        if request.method == "PATCH" && parts.first().is_some_and(|p| p == "config") {
            return Response::error(501, "live_configuration_updates_not_implemented");
        }
        if request.method != "POST" {
            return Response::error(405, "method_not_allowed");
        }
        let p: Vec<_> = parts.iter().map(String::as_str).collect();
        let body = request.body;
        match p.as_slice() {
            ["threads", id, action] => {
                if authority != Authority::Owner && !matches!(*action, "pause" | "resume") {
                    return Response::error(403, "owner_required");
                }
                let allowed = match *action {
                    "instruct" => vec!["actor", "text", "client_id"],
                    "notes" => vec!["actor", "data", "expected"],
                    "pause" => vec!["actor", "reason"],
                    _ => vec!["actor"],
                };
                if body
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| !allowed.contains(&k.as_str()))
                {
                    return Response::error(400, "unknown_body_field");
                }
                if !matches!(
                    *action,
                    "pause"
                        | "resume"
                        | "close"
                        | "archive"
                        | "instruct"
                        | "notes"
                        | "restore"
                        | "clean"
                ) {
                    return Response::error(404, "unknown_thread_action");
                }
                let id = id.to_string();
                let lookup = id.clone();
                match store
                    .call(move |c| {
                        Ok(
                            c.query_row("SELECT 1 FROM threads WHERE id=?", [lookup], |r| {
                                r.get::<_, i64>(0)
                            })
                            .optional()?
                            .is_some(),
                        )
                    })
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Response::error(404, "no_such_thread"),
                    Err(_) => return Response::error(500, "storage_failed"),
                }
                if matches!(*action, "restore" | "clean") {
                    return Response::error(501, "thread_restoration_and_cleaning_not_implemented");
                }
                if *action == "instruct" {
                    let (Some(text), Some(client_id)) =
                        (body["text"].as_str(), body["client_id"].as_str())
                    else {
                        return Response::error(400, "invalid_instruction");
                    };
                    if !(1..=4000).contains(&text.trim().chars().count())
                        || !(8..=80).contains(&client_id.len())
                        || !client_id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                    {
                        return Response::error(400, "invalid_instruction");
                    }
                    return match self
                        .runtime
                        .instruct(id, text.into(), client_id.into(), authority)
                        .await
                    {
                        Ok(id) => Response::ok(json!({"instruction_id":id,"queued":true})),
                        Err(error) => operation_error(error),
                    };
                }
                if *action == "notes" {
                    let Some(data) = body["data"].as_object() else {
                        return Response::error(400, "notes_must_be_an_object");
                    };
                    let data = data.clone();
                    let expected = match body.get("expected") {
                        None | Some(Value::Null) => None,
                        Some(v) => match v.as_u64().filter(|n| *n <= i64::MAX as u64) {
                            Some(n) => Some(n as i64),
                            None => return Response::error(400, "invalid_revision"),
                        },
                    };
                    let actor = config.owner.slack_user.clone();
                    let result = store.call(move |c| {
                        let tx = c.transaction()?;
                        let revision: i64 = tx.query_row(
                            "SELECT COALESCE(MAX(revision),0) FROM notes WHERE session_id=?",
                            [&id], |r| r.get(0),
                        )?;
                        if expected.is_some_and(|r| r != revision) {
                            anyhow::bail!("notes revision conflict");
                        }
                        let next = revision.checked_add(1).ok_or_else(|| anyhow::anyhow!("revision overflow"))?;
                        tx.execute(
                            "INSERT INTO notes(session_id,revision,actor,data_json,source,created) VALUES(?,?,?,?,'control',?)",
                            params![id,next,actor,json!(data).to_string(),now],
                        )?;
                        tx.execute(
                            "INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,'notes.write',?,?)",
                            params![now,actor,id,json!({"revision":next}).to_string()],
                        )?;
                        tx.commit()?;
                        Ok(next)
                    }).await;
                    return match result {
                        Ok(revision) => Response::ok(json!({"revision":revision})),
                        Err(error) => operation_error(error),
                    };
                }
                let control = match *action {
                    "pause" => {
                        if body.get("reason").is_some_and(|v| !v.is_string()) {
                            return Response::error(400, "invalid_pause_reason");
                        }
                        let reason = body.get("reason").and_then(Value::as_str).unwrap_or(
                            if authority == Authority::Owner {
                                "Paused by the owner."
                            } else {
                                "Paused by the overseer."
                            },
                        );
                        if reason.trim().is_empty() || reason.chars().count() > 4000 {
                            return Response::error(400, "invalid_pause_reason");
                        }
                        Control::Pause {
                            reason: reason.into(),
                        }
                    }
                    "resume" => Control::Resume,
                    "close" => Control::Close,
                    "archive" => Control::Archive,
                    _ => unreachable!(),
                };
                match self.runtime.control(id.clone(), control, authority).await {
                    Ok(()) => Response::ok(
                        json!({"session":id,"action":action,"queued":true,"applied":true}),
                    ),
                    Err(error) => operation_error(error),
                }
            }
            ["workers", id, op] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if !matches!(*op, "stop" | "interrupt") {
                    return Response::error(404, "unknown_worker_operation");
                }
                if !only_actor(&body) {
                    return Response::error(400, "unknown_body_field");
                }
                if let Err(error) = crate::store::work::get_worker(&store, id.to_string()).await {
                    return if error
                        .downcast_ref::<rusqlite::Error>()
                        .is_some_and(|e| matches!(e, rusqlite::Error::QueryReturnedNoRows))
                    {
                        Response::error(404, "no_such_worker")
                    } else {
                        operation_error(error)
                    };
                }
                match self
                    .runtime
                    .worker_control(id, *op == "stop", authority)
                    .await
                {
                    Ok(value) => Response::ok(if *op == "stop" {
                        json!({"stopped":value})
                    } else {
                        json!({"interrupted":value})
                    }),
                    Err(error) => operation_error(error),
                }
            }
            ["approvals", id] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if body
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k != "actor" && k != "decision")
                {
                    return Response::error(400, "unknown_body_field");
                }
                let decision = match body["decision"].as_str() {
                    Some("once") => ApprovalDecision::Once,
                    Some("session") => ApprovalDecision::Session,
                    Some("deny") => ApprovalDecision::Deny,
                    _ => return Response::error(400, "invalid_approval_decision"),
                };
                let id = id.to_string();
                let lookup = id.clone();
                match store
                    .call(move |c| {
                        Ok(
                            c.query_row("SELECT 1 FROM approvals WHERE id=?", [lookup], |r| {
                                r.get::<_, i64>(0)
                            })
                            .optional()?
                            .is_some(),
                        )
                    })
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Response::error(404, "no_such_approval"),
                    Err(_) => return Response::error(500, "storage_failed"),
                }
                match self.runtime.approvals.decide(id, decision, authority).await {
                    Ok(true) => Response::ok(json!({"decided":body["decision"]})),
                    Ok(false) => Response::error(409, "approval_already_settled"),
                    Err(error) => operation_error(error),
                }
            }
            ["outbox", id, "retry"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if !only_actor(&body) {
                    return Response::error(400, "unknown_body_field");
                }
                let Ok(id) = id.parse::<i64>() else {
                    return Response::error(400, "invalid_outbox_id");
                };
                match outbox::requeue(&store, id, authority, now).await {
                    Ok(true) => Response::ok(json!({"requeued":true})),
                    Ok(false) => Response::error(409, "post_not_retryable"),
                    Err(error) => operation_error(error),
                }
            }
            _ => Response::error(404, "not_found"),
        }
    }
}
fn only_actor(body: &Value) -> bool {
    body.as_object()
        .is_some_and(|v| v.keys().all(|k| k == "actor"))
}
fn operation_error(error: anyhow::Error) -> Response {
    if error.downcast_ref::<rusqlite::Error>().is_some() {
        Response::error(500, "storage_failed")
    } else if error.to_string().contains("owner") || error.to_string().contains("authentication") {
        Response::error(403, "owner_required")
    } else if matches!(
        error.to_string().as_str(),
        "notes revision conflict"
            | "instruction ID reused with different text"
            | "closed threads require explicit restoration"
            | "instruction outside configured scope"
            | "instructions are unavailable in observe-only mode"
    ) {
        Response::error(409, "operation_conflict")
    } else {
        Response::error(500, "operation_failed")
    }
}
impl<P: Parent + 'static, D: Delivery + 'static> Backend for Api<P, D> {
    fn request(&self, r: Request, a: Authority) -> AdapterFuture<'_, Response> {
        Box::pin(self.handle(r, a))
    }
}
pub fn target(value: &str) -> Option<(Vec<String>, BTreeMap<String, String>)> {
    if value.len() > 4096
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('#')
    {
        return None;
    }
    let (path, query) = value.split_once('?').unwrap_or((value, ""));
    let mut parts = vec![];
    for part in path[1..].split('/') {
        let mut bytes = vec![];
        let mut source = part.bytes();
        while let Some(c) = source.next() {
            if c == b'%' {
                let a = (source.next()? as char).to_digit(16)?;
                let b = (source.next()? as char).to_digit(16)?;
                bytes.push((a * 16 + b) as u8);
            } else {
                bytes.push(c);
            }
        }
        if bytes.is_empty()
            || bytes == b"."
            || bytes == b".."
            || bytes.len() > 512
            || bytes
                .iter()
                .any(|c| !c.is_ascii_graphic() || b"/\\?#".contains(c))
        {
            return None;
        }
        parts.push(String::from_utf8(bytes).ok()?);
    }
    let url = reqwest::Url::parse(&format!("http://fridica/?{query}")).ok()?;
    let mut fields = BTreeMap::new();
    for (k, v) in url.query_pairs() {
        if !["limit", "control", "status", "state"].contains(&k.as_ref())
            || v.len() > 200
            || fields.insert(k.into_owned(), v.into_owned()).is_some()
        {
            return None;
        }
    }
    if fields
        .get("limit")
        .is_some_and(|v| !v.parse::<usize>().is_ok_and(|n| (1..=1000).contains(&n)))
    {
        return None;
    }
    Some((parts, fields))
}
