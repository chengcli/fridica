//! One SQLite transaction per response. Never select blobs or private replay data.
use crate::config::Config;
use anyhow::{bail, Result};
use rusqlite::{types::ValueRef, Connection, Params};
use serde_json::{json, Value};
use std::collections::BTreeMap;
const THREAD:&str="SELECT id,workspace,channel,root_ts,status,control,pause_reason,turns,wait_streak,no_progress,last_reply_hash,reset_at,summary,decisions_json,context_json,debriefed_turn,last_unsolicited,created,updated,version,control_json AS control_detail_json,throttled_until FROM threads";
const WORKER:&str="SELECT id,session_id,machine,workspace,backend,role,ephemeral,backend_session_id,status,summary,last_result_json,slot,created,updated FROM workers";
const JOB:&str="SELECT id,worker_id,session_id,brief,join_group,inbox_id,deliverable,fetch_repo,fetch_ref,status,attempt,reported,result_json,error,queued_at,started_at,finished_at,work_item_id,target_sha,target_tree FROM jobs";
const OUTBOX:&str="SELECT id,idem_key,session_id,kind,channel,thread_ts,text,meta_json,filename,\"after\",state,attempts,retry_at,sent_ts,error,created,blob IS NOT NULL AS has_file,delivered_at,trigger_event,trigger_class,answers_json FROM outbox";
fn rows(c: &Connection, sql: &str, params: impl Params) -> Result<Vec<Value>> {
    let mut statement = c.prepare(sql)?;
    let columns = statement
        .column_names()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();
    let mut rows = statement.query(params)?;
    let mut result = vec![];
    let mut size = 0;
    while let Some(row) = rows.next()? {
        let mut value = serde_json::Map::new();
        for (i, name) in columns.iter().enumerate() {
            let raw = match row.get_ref(i)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(n) => json!(n),
                ValueRef::Real(n) => json!(n),
                ValueRef::Text(t) => json!(std::str::from_utf8(t)?),
                ValueRef::Blob(_) => bail!("binary fields are not control views"),
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
fn threads(mut values: Vec<Value>) -> Vec<Value> {
    for v in &mut values {
        v["key"] =
            json!({"workspace":v["workspace"],"channel":v["channel"],"root_ts":v["root_ts"]});
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
fn filter(column: &str, values: Option<&str>, default: &str) -> (String, Vec<String>) {
    let values = values
        .unwrap_or(default)
        .split(',')
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if values.is_empty() {
        return (String::new(), values);
    }
    (
        format!(" WHERE {column} IN ({})", vec!["?"; values.len()].join(",")),
        values,
    )
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
    let values = match parts.as_slice() {
        ["status"] => {
            let runtime = rows(c, "SELECT started_at,slack_status FROM runtime WHERE id=1", [])?
                .pop().unwrap_or(json!({"started_at":0.,"slack_status":"stopped"}));
            let count = |sql: &str| c.query_row(sql, [], |r| r.get::<_, i64>(0));
            return Ok(Some(json!({
                "owner":config.owner.slack_user,
                "started_at":runtime["started_at"],
                "observe_only":observe_only,
                "slack":runtime["slack_status"],
                "pending_approvals":count("SELECT count(*) FROM approvals WHERE status='pending'")?,
                "running_jobs":count("SELECT count(*) FROM jobs WHERE status='running'")?,
                "queued_jobs":count("SELECT count(*) FROM jobs WHERE status='queued'")?,
                "problem_posts":count("SELECT count(*) FROM outbox WHERE state IN ('failed','ambiguous','blocked')")?,
                "config":{"path":config.path,"fingerprint":config.fingerprint}
            })));
        }
        ["threads"] => {
            let (clause, values) = filter("control", query.get("control").map(String::as_str), "");
            threads(rows(c, &format!("{THREAD}{clause} ORDER BY updated DESC,id LIMIT {limit}"),
                rusqlite::params_from_iter(values))?)
        }
        ["attention", "threads"] => threads(rows(c,
            &format!("{THREAD} WHERE control='paused' OR (control='active' AND status='blocked') ORDER BY updated DESC,id"), [])?),
        ["threads", id] => return thread(c, id, query, processes),
        ["workers"] => {
            let (clause, values) = filter("status", query.get("status").map(String::as_str), "");
            workers(rows(c, &format!("{WORKER}{clause} ORDER BY updated DESC,id LIMIT {limit}"),
                rusqlite::params_from_iter(values))?, processes)
        }
        ["jobs"] if query.get("status").is_some_and(|value| value == "all") => rows(c,
            &format!("{JOB} ORDER BY CASE WHEN status IN ('running','queued') THEN 0 ELSE 1 END, \
                queued_at DESC,rowid DESC LIMIT {limit}"), [])?,
        ["jobs"] => rows(c, &format!("{JOB} WHERE status IN ('running','queued') ORDER BY \
            CASE status WHEN 'running' THEN 0 ELSE 1 END, \
            CASE status WHEN 'running' THEN started_at ELSE queued_at END,rowid LIMIT {limit}"), [])?,
        ["approvals"] => {
            let (clause, values) = filter("status", query.get("status").map(String::as_str), "pending");
            if values.is_empty() { return Ok(Some(json!([]))); }
            rows(c, &format!("SELECT id,worker_id,job_id,session_id,backend_request_id,kind,summary,\
                detail_json,status,scope,decided_by,created,decided_at,expires_at FROM approvals\
                {clause} ORDER BY created DESC,id LIMIT {limit}"), rusqlite::params_from_iter(values))?
        }
        ["outbox"] => {
            let (clause, values) = filter("state", query.get("state").map(String::as_str), "failed,ambiguous");
            if values.is_empty() { return Ok(Some(json!([]))); }
            rows(c, &format!("{OUTBOX}{clause} ORDER BY id DESC LIMIT {limit}"), rusqlite::params_from_iter(values))?
        }
        ["activity"] => rows(c, &format!("SELECT id,time,actor,action,target,details_json FROM audit ORDER BY id DESC LIMIT {limit}"), [])?,
        ["obligations"] => rows(c, &format!("SELECT id,session_id,kind,summary,created,due,state,\
            state_json AS disposition_json,updated FROM obligations ORDER BY created DESC,id LIMIT {limit}"), [])?,
        ["config"] => return Ok(Some(json!({
            "parent":{"backend":config.parent.backend,"model":config.parent.model,
                "triage_model":config.parent.triage_model,"reasoning_effort":config.parent.reasoning_effort},
            "limits":config.limits,"attention":config.attention
        }))),
        ["machines"] => machines(c, config, processes)?,
        _ => return Ok(None),
    };
    Ok(Some(json!(values)))
}
fn thread(
    c: &Connection,
    id: &str,
    query: &BTreeMap<String, String>,
    processes: &BTreeMap<String, String>,
) -> Result<Option<Value>> {
    let Some(session) = threads(rows(c, &format!("{THREAD} WHERE id=?"), [id])?).pop() else {
        return Ok(None);
    };
    let limit = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200);
    let mut messages = rows(
        c,
        "SELECT event_id,ts,thread_ts,sender,text,source,meta_json FROM messages \
        WHERE workspace||':'||channel||':'||root_ts=? ORDER BY CAST(ts AS REAL) DESC LIMIT ?",
        rusqlite::params![id, limit],
    )?;
    messages.reverse();
    for message in &mut messages {
        message["generated"] = json!(!message["meta"].is_null());
    }
    let notes = rows(
        c,
        "SELECT revision,data_json FROM notes WHERE session_id=? ORDER BY revision DESC LIMIT 1",
        [id],
    )?
    .pop()
    .unwrap_or(json!({"revision":0,"data":{}}));
    let mut instructions = rows(
        c,
        "SELECT id,ref,created,state,payload_json FROM thread_inbox \
        WHERE session_id=? AND kind='owner_instruction' ORDER BY id DESC LIMIT 20",
        [id],
    )?;
    for item in &mut instructions {
        item["text"] = item["payload"]["text"].clone();
        item.as_object_mut().unwrap().remove("payload");
    }
    let workers = workers(
        rows(
            c,
            &format!("{WORKER} WHERE session_id=? ORDER BY created,rowid"),
            [id],
        )?,
        processes,
    );
    let jobs = rows(
        c,
        &format!(
            "{JOB} WHERE session_id=? ORDER BY \
        (SELECT created FROM workers WHERE workers.id=jobs.worker_id),\
        (SELECT rowid FROM workers WHERE workers.id=jobs.worker_id),queued_at,rowid"
        ),
        [id],
    )?;
    let outbox = rows(c, &format!("{OUTBOX} WHERE session_id=? ORDER BY id"), [id])?;
    Ok(Some(
        json!({"session":session,"messages":messages,"workers":workers,
        "jobs":jobs,"outbox":outbox,"instructions":instructions,"notes":notes,"worker_controls":crate::store::worker_controls::recent_tx(c,id)?}),
    ))
}
fn machines(
    c: &Connection,
    config: &Config,
    processes: &BTreeMap<String, String>,
) -> Result<Vec<Value>> {
    let counts = rows(
        c,
        "SELECT machine,count(*) AS busy FROM workers w JOIN jobs j ON j.worker_id=w.id \
        WHERE j.status IN ('queued','running') GROUP BY machine",
        [],
    )?;
    let worker_rows = rows(c, "SELECT id,machine FROM workers", [])?;
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
