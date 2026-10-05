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
    slack::names::Names,
    threads::{
        controls::Control,
        external::{DelegateRequest, PostRequest, Refusal, Target},
        runtime::Runtime,
    },
};
use fridica_core::fork::ContextMode;
use fridica_core::store::{MessageFiles, Store as _};
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
        if !matches!(authority, Authority::Owner | Authority::DesktopReadOnly) {
            return Response::error(403, "forbidden");
        }
        let Some((mut parts, query)) = target(&request.target) else {
            return Response::error(400, "invalid_target");
        };
        let store = self.runtime.store();
        // Owners may name a thread `#channel:TS`; state uses the full thread ID.
        if parts.len() >= 2 && parts[0] == "threads" && parts[1].matches(':').count() == 1 {
            let reference = parts[1].clone();
            let slack = self.runtime.config().slack.clone();
            match store
                .transact(move |u| Ok(Names::recorded(u, &slack)?.resolve(&reference)))
                .await
            {
                Ok(Some(id)) => parts[1] = id,
                Ok(None) => return Response::error(404, "unknown_thread"),
                Err(_) => return Response::error(500, "view_unavailable"),
            }
        }
        let now = self.runtime.now();
        if !now.is_finite() {
            return Response::error(500, "invalid_control_clock");
        }
        let record = json!({"method":request.method,"target":request.target,"body":request.body,"authority":authority});
        let call = match store
            .transact(move |u| u.record("control_request", now, &record.to_string(), false))
            .await
        {
            Ok(id) => id,
            Err(_) => return Response::error(500, "control_recording_failed"),
        };
        let response = self.route(request, authority, &parts, query).await;
        let now = self.runtime.now();
        let record = json!({"call":call,"status":response.status,"body":response.body});
        let recorded = store
            .transact(move |u| {
                u.record("control_response", now, &record.to_string(), true)?;
                u.complete(call, true)
            })
            .await;
        if recorded.is_err() {
            return Response::error(500, "control_outcome_unrecorded");
        }
        response
    }
    /// The event feed (docs/events.md): `GET /events?after=<cursor>&limit=N`
    /// scans the ledger after `after`; without `after` it only says where the
    /// ledger ends, so a new follower can start from now.
    async fn events(&self, authority: Authority, query: &BTreeMap<String, String>) -> Response {
        use super::events;
        if authority != Authority::Owner {
            return Response::error(403, "owner_required");
        }
        let after = match query.get("after").map(|v| v.parse::<i64>()) {
            None => None,
            Some(Ok(n)) if n >= 0 => Some(n),
            Some(_) => return Response::error(400, "invalid_cursor"),
        };
        let limit = query
            .get("limit")
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(100);
        let slack = self.runtime.config().slack.clone();
        let result = self
            .runtime
            .store()
            .transact(move |u| match after {
                Some(after) => {
                    let names = Names::recorded(u, &slack)?;
                    events::read(u, &names, after, limit)
                }
                None => Ok(json!({"v":events::VERSION,"events":[],"next":events::end(u)?})),
            })
            .await;
        match result {
            Ok(value) => Response::ok(value),
            Err(_) => Response::error(500, "view_unavailable"),
        }
    }
    /// Owner-only Slack file reads for local tools: list a thread's files, or
    /// read one text file (64 KiB at most) through the daemon's own client.
    async fn files(
        &self,
        authority: Authority,
        parts: &[String],
        query: &BTreeMap<String, String>,
    ) -> Response {
        use crate::slack::files::{self, Attachment, Failure as FileFailure};
        if authority != Authority::Owner {
            return Response::error(403, "owner_required");
        }
        let store = self.runtime.store();
        match parts {
            [_] => {
                let Some(thread) = query.get("thread").cloned() else {
                    return Response::error(400, "thread_required");
                };
                let slack = self.runtime.config().slack.clone();
                let listed = store
                    .transact(move |u| {
                        let Some(id) = Names::recorded(u, &slack)?.resolve(&thread) else {
                            return Ok(None);
                        };
                        u.thread_files(&id)
                    })
                    .await;
                match listed {
                    Ok(Some(rows)) => {
                        let mut out = vec![];
                        for MessageFiles {
                            ts,
                            sender,
                            attachments,
                        } in rows
                        {
                            let values: Vec<Value> =
                                serde_json::from_str(&attachments).unwrap_or_default();
                            for value in values {
                                if let Ok(a) = serde_json::from_value::<Attachment>(value) {
                                    out.push(json!({"id":a.id,"name":a.name,"mimetype":a.mimetype,"size":a.size,"ts":ts,"sender":sender,"text":files::is_text(&a)}));
                                }
                            }
                        }
                        Response::ok(json!(out))
                    }
                    Ok(None) => Response::error(404, "unknown_thread"),
                    Err(_) => Response::error(500, "view_unavailable"),
                }
            }
            [_, id] => {
                if id.len() < 2
                    || id.len() > 64
                    || !id.starts_with('F')
                    || !id.bytes().all(|b| b.is_ascii_alphanumeric())
                {
                    return Response::error(400, "invalid_file");
                }
                let key = id.clone();
                let found = store
                    .transact(move |u| {
                        let rows = u.attachments_mentioning(&key)?;
                        Ok(rows
                            .iter()
                            .flat_map(|j| serde_json::from_str::<Vec<Value>>(j).unwrap_or_default())
                            .filter_map(|v| serde_json::from_value::<Attachment>(v).ok())
                            .find(|a| a.id == key))
                    })
                    .await;
                let attachment = match found {
                    Ok(Some(a)) => a,
                    Ok(None) => return Response::error(404, "unknown_file"),
                    Err(_) => return Response::error(500, "view_unavailable"),
                };
                if !files::is_text(&attachment) {
                    return Response::error(415, "not_text");
                }
                let Some(reader) = self.runtime.files() else {
                    return Response::error(503, "files_unavailable");
                };
                let html = attachment
                    .mimetype
                    .split(';')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .eq_ignore_ascii_case("text/html");
                let read = async {
                    let url = if attachment.url.is_empty() {
                        reader.resolve(attachment.id.clone()).await?
                    } else {
                        attachment.url.clone()
                    };
                    reader.download(url, html).await
                };
                match tokio::time::timeout(std::time::Duration::from_secs(30), read)
                    .await
                    .unwrap_or(Err(FileFailure::Timeout))
                {
                    Ok(download) if download.data.len() > files::FILE_LIMIT => {
                        Response::error(413, "file_too_large")
                    }
                    Ok(download) => {
                        use sha2::{Digest, Sha256};
                        let hex: String =
                            download.data.iter().map(|b| format!("{b:02x}")).collect();
                        Response::ok(
                            json!({"id":attachment.id,"name":attachment.name,"mimetype":attachment.mimetype,
                            "size":download.data.len(),"sha256":format!("{:x}",Sha256::digest(&download.data)),"hex":hex}),
                        )
                    }
                    Err(FileFailure::Recording) => Response::error(500, "recording_failed"),
                    Err(failure) => Response {
                        status: 502,
                        body: json!({"error":"file_unavailable","reason":failure.note()}),
                    },
                }
            }
            _ => Response::error(404, "not_found"),
        }
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
            if parts.first().is_some_and(|p| p == "files") {
                return self.files(authority, parts, &query).await;
            }
            if parts == ["events"] {
                return self.events(authority, &query).await;
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
                .transact(move |u| views::get(u, &config, &parts, &query, &processes, observe_only))
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
            if authority != Authority::Owner {
                return Response::error(403, "owner_required");
            }
            if parts.len() != 2 || !matches!(parts[1].as_str(), "parent" | "limits") {
                return Response::error(404, "not_found");
            }
            if !crate::config::editor::valid_patch(&parts[1], &request.body) {
                return Response::error(400, "configuration_field_not_editable");
            }
            if !self.runtime.configuration_editable() {
                return Response::error(503, "configuration_editor_unavailable");
            }
            return match self
                .runtime
                .update_configuration(&parts[1], request.body, authority)
                .await
            {
                Ok(config) => {
                    let value = if parts[1] == "parent" {
                        json!({"parent":{"backend":config.parent.backend,"model":config.parent.model,"triage_model":config.parent.triage_model,"reasoning_effort":config.parent.reasoning_effort}})
                    } else {
                        json!({"limits":config.limits})
                    };
                    Response::ok(value)
                }
                Err(error) if error.is::<crate::config::editor::InvalidPatch>() => {
                    Response::error(400, "invalid_configuration_values")
                }
                Err(_) => Response::error(409, "configuration_update_refused_or_pending"),
            };
        }
        if request.method != "POST" {
            return Response::error(405, "method_not_allowed");
        }
        let p: Vec<_> = parts.iter().map(String::as_str).collect();
        let body = request.body;
        match p.as_slice() {
            ["obligations", id, "close"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let Some(reason) = body
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty() && s.chars().count() <= 4000)
                else {
                    return Response::error(400, "invalid_reason");
                };
                if body.as_object().unwrap().len() != 1 {
                    return Response::error(400, "unknown_body_field");
                }
                match crate::attention::disposition(
                    &store,
                    id.to_string(),
                    crate::attention::Disposition::OwnerClosed {
                        reason: reason.into(),
                    },
                    authority,
                    now,
                )
                .await
                {
                    Ok(()) => Response::ok(json!({"id":id,"state":"owner_closed"})),
                    Err(_) => Response::error(409, "obligation_close_refused"),
                }
            }
            ["archive", "restore"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let Some(thread) = body
                    .get("thread")
                    .and_then(Value::as_str)
                    .filter(|s| s.matches(':').count() == 2 && s.len() <= 200)
                    .map(str::to_owned)
                else {
                    return Response::error(400, "invalid_thread");
                };
                if body.as_object().unwrap().len() != 1 {
                    return Response::error(400, "unknown_body_field");
                }
                let id = thread.clone();
                match store.transact(move |u| u.revive_thread(&id, now)).await {
                    Ok(true) => Response::ok(json!({"thread":thread,"restored":true})),
                    Ok(false) => Response::error(404, "not_archived"),
                    Err(_) => Response::error(409, "restore_failed"),
                }
            }
            ["obligations", "backfill"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let Ok(request) =
                    serde_json::from_value::<crate::attention::backfill::Request>(body)
                else {
                    return Response::error(400, "invalid_backfill_request");
                };
                if !request.valid(now) {
                    return Response::error(400, "invalid_backfill_request");
                }
                match crate::attention::backfill::run(&store, &config, request, authority, now)
                    .await
                {
                    Ok(result) => Response::ok(result),
                    Err(_) => Response::error(409, "backfill_refused"),
                }
            }
            ["channels", channel, "instruct"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if body
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| !["actor", "text", "client_id"].contains(&k.as_str()))
                {
                    return Response::error(400, "unknown_body_field");
                }
                // A configured channel ID, or its name as recorded at Slack startup.
                let wanted = channel.to_string();
                let slack = config.slack.clone();
                let thread = store
                    .transact(move |u| {
                        let names = Names::recorded(u, &slack)?;
                        let Some(id) = names.channel_id(&wanted) else {
                            return Ok(Err("unknown_channel"));
                        };
                        Ok(u.latest_thread_in(&slack.workspace, &id)?
                            .map(|thread| (names.thread(&thread), thread))
                            .ok_or("no_thread_in_channel"))
                    })
                    .await;
                let (name, thread) = match thread {
                    Ok(Ok(thread)) => thread,
                    Ok(Err(code)) => return Response::error(404, code),
                    Err(_) => return Response::error(500, "storage_failed"),
                };
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
                match self
                    .runtime
                    .instruct(thread.clone(), text.into(), client_id.into(), authority)
                    .await
                {
                    // Like every view: `thread` is the readable name, `id` the key.
                    Ok(id) => Response::ok(
                        json!({"instruction_id":id,"queued":true,"thread":name,"id":thread}),
                    ),
                    Err(error) => operation_error(error),
                }
            }
            ["threads", id, "delegate"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let request = match delegate_body(&body) {
                    Ok(request) => request,
                    Err(code) => return Response::error(400, code),
                };
                answer(
                    self.runtime
                        .delegate(id.to_string(), request, authority)
                        .await,
                )
            }
            ["threads", id, "post"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let request = match post_body(&body, false) {
                    Ok(request) => request,
                    Err(code) => return Response::error(400, code),
                };
                answer(
                    self.runtime
                        .external_post(Target::Thread(id.to_string()), request, authority)
                        .await,
                )
            }
            ["channels", channel, "post"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                let request = match post_body(&body, true) {
                    Ok(request) => request,
                    Err(code) => return Response::error(400, code),
                };
                answer(
                    self.runtime
                        .external_post(Target::Channel(channel.to_string()), request, authority)
                        .await,
                )
            }
            ["threads", id, "driver"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if body
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| !["actor", "driver"].contains(&k.as_str()))
                {
                    return Response::error(400, "unknown_body_field");
                }
                let Some(driver) = body["driver"]
                    .as_str()
                    .filter(|d| matches!(*d, "parent" | "external"))
                    .map(str::to_owned)
                else {
                    return Response::error(400, "invalid_driver");
                };
                let (session, actor, value) =
                    (id.to_string(), json!(authority).to_string(), driver.clone());
                match store
                    .transact(move |u| {
                        if !u.thread_exists(&session)? {
                            return Ok(None);
                        }
                        u.set_thread_driver(&session, &value, &actor, now).map(Some)
                    })
                    .await
                {
                    Ok(Some(changed)) => Response::ok(json!({"driver":driver,"changed":changed})),
                    Ok(None) => Response::error(404, "no_such_thread"),
                    Err(error) => operation_error(error),
                }
            }
            ["threads", id, "workers", worker, "stop"] => {
                if authority != Authority::Owner {
                    return Response::error(403, "owner_required");
                }
                if body
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| !["actor", "mode"].contains(&k.as_str()))
                {
                    return Response::error(400, "unknown_body_field");
                }
                let stop = match body.get("mode") {
                    None | Some(Value::Null) => true,
                    Some(mode) => match mode.as_str() {
                        Some("stop") => true,
                        Some("interrupt") => false,
                        _ => return Response::error(400, "invalid_mode"),
                    },
                };
                let (session, lookup) = (id.to_string(), worker.to_string());
                match store
                    .transact(move |u| {
                        Ok(u.thread_workers(&session)?.iter().any(|w| w.id == lookup))
                    })
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return Response::error(404, "no_such_worker"),
                    Err(error) => return operation_error(error),
                }
                match self.runtime.worker_control(worker, stop, authority).await {
                    Ok(value) => Response::ok(if stop {
                        json!({"stopped":value})
                    } else {
                        json!({"interrupted":value})
                    }),
                    Err(error) => operation_error(error),
                }
            }
            ["threads", id, action] => {
                if authority != Authority::Owner {
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
                match store.transact(move |u| u.thread_exists(&lookup)).await {
                    Ok(true) => {}
                    Ok(false) => return Response::error(404, "no_such_thread"),
                    Err(_) => return Response::error(500, "storage_failed"),
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
                    let slack = config.slack.clone();
                    let key = id.clone();
                    let name = store
                        .transact(move |u| Ok(Names::recorded(u, &slack)?.thread(&key)))
                        .await
                        .unwrap_or_else(|_| id.clone());
                    return match self
                        .runtime
                        .instruct(id.clone(), text.into(), client_id.into(), authority)
                        .await
                    {
                        Ok(instruction) => Response::ok(
                            json!({"instruction_id":instruction,"queued":true,"thread":name,"id":id}),
                        ),
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
                    let result = store
                        .transact(move |u| {
                            let revision = u.notes_revision(&id)?;
                            if expected.is_some_and(|r| r != revision) {
                                anyhow::bail!("notes revision conflict");
                            }
                            let next = revision
                                .checked_add(1)
                                .ok_or_else(|| anyhow::anyhow!("revision overflow"))?;
                            u.write_owner_notes(&id, next, &actor, &json!(data).to_string(), now)?;
                            Ok(next)
                        })
                        .await;
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
                        let reason = body
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("Paused by the owner.");
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
                    "restore" => Control::Restore,
                    "clean" => Control::Clean,
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
                let worker = id.to_string();
                if let Err(error) = store.transact(move |u| u.worker_record(&worker)).await {
                    return if crate::store::not_found(&error) {
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
                match store.transact(move |u| u.approval_exists(&lookup)).await {
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
                match store
                    .transact(move |u| u.retry_post(id, authority, now))
                    .await
                {
                    Ok(true) => Response::ok(json!({"requeued":true})),
                    Ok(false) => Response::error(409, "post_not_retryable"),
                    Err(error) => operation_error(error),
                }
            }
            _ => Response::error(404, "not_found"),
        }
    }
}
/// A refusal is its status and code; anything else failed.
fn answer(result: anyhow::Result<std::result::Result<Value, Refusal>>) -> Response {
    match result {
        Ok(Ok(value)) => Response::ok(value),
        Ok(Err((status, code))) => Response::error(status, code),
        Err(error) => operation_error(error),
    }
}
/// The body of `POST /threads/<id>/delegate`, checked strictly.
fn delegate_body(body: &Value) -> Result<DelegateRequest, &'static str> {
    const FIELDS: [&str; 9] = [
        "actor",
        "role",
        "brief",
        "context",
        "worker_id",
        "ephemeral",
        "backend",
        "deliverable",
        "tags",
    ];
    let fields = body.as_object().ok_or("invalid_body_or_query")?;
    if fields.keys().any(|k| !FIELDS.contains(&k.as_str())) {
        return Err("unknown_body_field");
    }
    // An absent or null field takes its default.
    let field = |name: &str| fields.get(name).filter(|v| !v.is_null());
    let string = |name: &str, default: &str, code| match field(name) {
        None => Ok(default.to_owned()),
        Some(v) => v.as_str().map(str::to_owned).ok_or(code),
    };
    let brief = string("brief", "", "invalid_brief")?;
    if brief.trim().is_empty() || brief.chars().count() > 40000 {
        return Err("invalid_brief");
    }
    let context = match string("context", "fresh", "invalid_context")?.as_str() {
        "fresh" => ContextMode::Fresh,
        "fork" => ContextMode::Fork,
        _ => return Err("invalid_context"),
    };
    let ephemeral = match field("ephemeral") {
        None => false,
        Some(v) => v.as_bool().ok_or("invalid_ephemeral")?,
    };
    let tags = match field("tags") {
        None => vec![],
        Some(v) => v
            .as_array()
            .filter(|tags| tags.len() <= 16)
            .ok_or("invalid_tags")?
            .iter()
            .map(|tag| {
                tag.as_str()
                    .filter(|t| !t.is_empty() && t.chars().count() <= 200)
                    .map(str::to_owned)
                    .ok_or("invalid_tags")
            })
            .collect::<Result<_, _>>()?,
    };
    let backend = string("backend", "same", "invalid_backend")?;
    if backend.is_empty() || backend.len() > 64 {
        return Err("invalid_backend");
    }
    Ok(DelegateRequest {
        role: string("role", "general", "invalid_role")?,
        brief,
        context,
        worker_id: string("worker_id", "", "unknown_worker")?,
        ephemeral,
        backend,
        deliverable: string("deliverable", "report", "invalid_deliverable")?,
        tags,
    })
}
/// The body of a driver's post, checked strictly; a channel takes only a
/// `study_root`, and a root carries no details (it has no thread yet).
fn post_body(body: &Value, channel: bool) -> Result<PostRequest, &'static str> {
    let fields = body.as_object().ok_or("invalid_body_or_query")?;
    if fields
        .keys()
        .any(|k| !["actor", "text", "details", "meta", "client_id"].contains(&k.as_str()))
    {
        return Err("unknown_body_field");
    }
    let text = body["text"].as_str().ok_or("invalid_text")?;
    if text.trim().is_empty() || text.chars().count() > 40000 {
        return Err("invalid_text");
    }
    let details = match fields.get("details").filter(|v| !v.is_null()) {
        None => "",
        Some(v) => v
            .as_str()
            .filter(|d| d.chars().count() <= 40000)
            .ok_or("invalid_details")?,
    };
    let meta = body["meta"].as_object().ok_or("invalid_meta")?;
    if meta.keys().any(|k| k != "kind" && k != "status") {
        return Err("unknown_meta_field");
    }
    let kind = meta
        .get("kind")
        .and_then(Value::as_str)
        .filter(|k| crate::threads::external::outbox_kind(k).is_some())
        .ok_or("invalid_post_kind")?;
    if channel && kind != "study_root" {
        return Err("invalid_post_kind");
    }
    if kind == "study_root" && !details.is_empty() {
        return Err("invalid_details");
    }
    let status = match meta.get("status").filter(|v| !v.is_null()) {
        None => "complete",
        Some(v) => v
            .as_str()
            .filter(|s| matches!(*s, "complete" | "waiting" | "blocked"))
            .ok_or("invalid_status")?,
    };
    let client_id = match fields.get("client_id").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_str()
                .filter(|id| {
                    (8..=80).contains(&id.len())
                        && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
                })
                .ok_or("invalid_client_id")?
                .to_owned(),
        ),
    };
    Ok(PostRequest {
        kind: kind.into(),
        status: status.into(),
        text: text.into(),
        details: details.into(),
        client_id,
    })
}
fn only_actor(body: &Value) -> bool {
    body.as_object()
        .is_some_and(|v| v.keys().all(|k| k == "actor"))
}
fn operation_error(error: anyhow::Error) -> Response {
    if crate::store::storage_failure(&error) {
        Response::error(500, "storage_failed")
    } else if error.to_string().contains("owner") || error.to_string().contains("authentication") {
        Response::error(403, "owner_required")
    } else if error.to_string() == "thread worker cleanup is pending" {
        Response::error(409, "thread_cleanup_pending")
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
        if !["limit", "control", "status", "state", "thread", "after"].contains(&k.as_ref())
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
