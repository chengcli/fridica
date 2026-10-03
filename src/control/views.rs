//! One SQLite transaction per response. Never select blobs or private replay data.
use crate::{config::Config, slack::names::Names, store::Sqlite};
use anyhow::{bail, Result};
use fridica_core::store::{Cell, Row, Views};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;
fn rows(raw: Vec<Row>) -> Result<Vec<Value>> {
    let mut result = vec![];
    let mut size = 0;
    for Row(cells) in raw {
        let mut value = serde_json::Map::new();
        for (name, cell) in cells {
            let raw = match cell {
                Cell::Null => Value::Null,
                Cell::Integer(n) => json!(n),
                Cell::Real(n) => json!(n),
                Cell::Text(t) => json!(t),
            };
            let (key, data) = if let Some(key) = name.strip_suffix("_json") {
                (
                    key,
                    if raw.is_null() {
                        Value::Null
                    } else {
                        serde_json::from_str(raw.as_str().unwrap())?
                    },
                )
            } else if ["ephemeral", "reported", "has_file", "observe_only"].contains(&name.as_str())
            {
                (name.as_str(), json!(raw.as_i64().unwrap_or(0) != 0))
            } else {
                (name.as_str(), raw)
            };
            value.insert(key.into(), data);
        }
        for key in ["result", "last_result"] {
            if let Some(result) = value.get_mut(key).filter(|v| !v.is_null()) {
                let typed: crate::core::worker::WorkerResult =
                    serde_json::from_value(result.clone())?;
                *result = serde_json::to_value(typed)?;
            }
        }
        if let Some(meta) = value.get_mut("meta").filter(|v| !v.is_null()) {
            let Some(fields) = meta.as_object() else {
                bail!("invalid control metadata");
            };
            let mut normalized =
                json!({"session":"", "turn":0, "status":"", "kind":"reply", "worker":"", "v":2});
            for (key, value) in fields {
                normalized[key] = value.clone();
            }
            *meta = normalized;
        }
        let value = Value::Object(value);
        size += serde_json::to_vec(&value)?.len();
        if size > super::RESPONSE_LIMIT {
            bail!("control view exceeds limit");
        }
        result.push(value);
    }
    Ok(result)
}
/// Adds the readable `#channel:TS` beside each thread reference.
fn label(values: &mut [Value], names: &Names) {
    for v in values {
        if let Some(session) = v["session_id"].as_str() {
            v["thread"] = json!(names.thread(session));
        }
    }
}
fn threads(mut values: Vec<Value>, names: &Names) -> Vec<Value> {
    for v in &mut values {
        v["name"] = json!(names.thread(v["id"].as_str().unwrap_or("")));
        v["key"] = json!({"workspace":v["workspace"],"workspace_name":names.workspace_name,
            "channel":v["channel"],"channel_name":names.channels.get(v["channel"].as_str().unwrap_or("")),
            "root_ts":v["root_ts"]});
        for k in ["workspace", "channel", "root_ts"] {
            v.as_object_mut().unwrap().remove(k);
        }
        let mut context = json!({"machine":"","workspace":"","repo":"","branch":"","backend":""});
        if let Some(map) = v["context"].as_object() {
            for (k, value) in map {
                context[k] = value.clone();
            }
        }
        v["context"] = context;
    }
    values
}
fn workers(mut values: Vec<Value>, processes: &BTreeMap<String, String>) -> Vec<Value> {
    for v in &mut values {
        v["process"] = json!(processes
            .get(v["id"].as_str().unwrap())
            .map(String::as_str)
            .unwrap_or("stopped"));
    }
    values
}
/// The values of a comma-separated filter; none means everything.
fn filter(values: Option<&str>, default: &str) -> Vec<String> {
    values
        .unwrap_or(default)
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect()
}
pub fn get(
    c: &Connection,
    config: &Config,
    parts: &[String],
    query: &BTreeMap<String, String>,
    processes: &BTreeMap<String, String>,
    observe_only: bool,
) -> Result<Option<Value>> {
    let limit = query
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(if parts == ["activity"] { 200 } else { 100 });
    let parts: Vec<_> = parts.iter().map(String::as_str).collect();
    let names = Names::load(c, &config.slack)?;
    let u = &mut Sqlite(c);
    let mut values = match parts.as_slice() {
        ["status"] => {
            let status = u.status()?;
            let runtime = rows(status.runtime.into_iter().collect())?
                .pop()
                .unwrap_or(json!({"started_at":0.,"slack_status":"stopped"}));
            return Ok(Some(json!({
                "owner":config.owner.slack_user,
                "started_at":runtime["started_at"],
                "observe_only":observe_only,
                "slack":runtime["slack_status"],
                "pending_approvals":status.pending_approvals,
                "running_jobs":status.running_jobs,
                "queued_jobs":status.queued_jobs,
                "problem_posts":status.problem_posts,
                "config":{"path":config.path,"fingerprint":config.fingerprint}
            })));
        }
        ["threads"] => {
            let controls = filter(query.get("control").map(String::as_str), "");
            threads(rows(u.threads(&controls, limit)?)?, &names)
        }
        ["attention", "threads"] => threads(rows(u.threads_needing_attention()?)?, &names),
        ["threads", id] => return thread(u, id, query, processes, &names),
        ["workers"] => {
            let statuses = filter(query.get("status").map(String::as_str), "");
            workers(rows(u.workers(&statuses, limit)?)?, processes)
        }
        ["jobs"] if query.get("status").is_some_and(|value| value == "all") => {
            rows(u.all_jobs(limit)?)?
        }
        ["jobs"] => rows(u.active_jobs(limit)?)?,
        ["approvals"] => {
            let statuses = filter(query.get("status").map(String::as_str), "pending");
            if statuses.is_empty() {
                return Ok(Some(json!([])));
            }
            rows(u.approvals(&statuses, limit)?)?
        }
        ["outbox"] => {
            let states = filter(query.get("state").map(String::as_str), "failed,ambiguous");
            if states.is_empty() {
                return Ok(Some(json!([])));
            }
            rows(u.posts(&states, limit)?)?
        }
        ["activity"] => rows(u.activity(limit)?)?,
        ["obligations"] => rows(u.obligations(limit)?)?,
        ["config"] => {
            return Ok(Some(json!({
                "parent":{"backend":config.parent.backend,"model":config.parent.model,
                    "triage_model":config.parent.triage_model,"reasoning_effort":config.parent.reasoning_effort},
                "limits":config.limits,"attention":config.attention
            })))
        }
        ["machines"] => machines(u, config, processes)?,
        _ => return Ok(None),
    };
    label(&mut values, &names);
    Ok(Some(json!(values)))
}
fn thread(
    u: &mut Sqlite,
    id: &str,
    query: &BTreeMap<String, String>,
    processes: &BTreeMap<String, String>,
    names: &Names,
) -> Result<Option<Value>> {
    let Some(session) = threads(rows(u.thread(id)?.into_iter().collect())?, names).pop() else {
        return Ok(None);
    };
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200);
    let mut messages = rows(u.thread_messages(id, limit)?)?;
    for message in &mut messages {
        message["generated"] = json!(!message["meta"].is_null());
    }
    let notes = rows(u.thread_notes(id)?.into_iter().collect())?
        .pop()
        .unwrap_or(json!({"revision":0,"data":{}}));
    let mut instructions = rows(u.owner_instructions(id)?)?;
    for item in &mut instructions {
        item["text"] = item["payload"]["text"].clone();
        item.as_object_mut().unwrap().remove("payload");
    }
    let workers = workers(rows(u.thread_workers(id)?)?, processes);
    let jobs = rows(u.thread_jobs(id)?)?;
    let outbox = rows(u.thread_posts(id)?)?;
    Ok(Some(
        json!({"session":session,"messages":messages,"workers":workers,
        "jobs":jobs,"outbox":outbox,"instructions":instructions,"notes":notes,"worker_controls":crate::store::worker_controls::recent_tx(u.0,id)?}),
    ))
}
fn machines(
    u: &mut Sqlite,
    config: &Config,
    processes: &BTreeMap<String, String>,
) -> Result<Vec<Value>> {
    let counts = rows(u.busy_machines()?)?;
    let worker_rows = rows(u.worker_machines()?)?;
    Ok(config
        .machines
        .machines
        .iter()
        .map(|machine| {
            let busy = counts
                .iter()
                .find(|v| v["machine"] == machine.name)
                .and_then(|v| v["busy"].as_u64())
                .unwrap_or(0) as usize;
            let mut value = machine.payload(busy);
            value["transport"] = json!(machine.transport);
            value["host"] = json!(machine.host);
            value["max_workers"] = json!(machine.max_workers);
            value["live_workers"] = json!(worker_rows
                .iter()
                .filter(|v| v["machine"] == machine.name
                    && processes.contains_key(v["id"].as_str().unwrap()))
                .count());
            value["workspace_details"] = json!(machine
                .workspaces
                .iter()
                .map(|w| json!({"name":w.name,"path":w.path,"mode":w.policy.mode,
                "approvals":w.policy.approvals,"network":w.policy.network}))
                .collect::<Vec<_>>());
            value
        })
        .collect())
}
