//! Frozen app-server JSON-RPC envelopes. Requests outside this allowlist decline.
use super::{
    jsonl::{Session, WireError},
    protocol::WorkerSpec,
    result,
};
use crate::core::worker::{ApprovalDecision, ApprovalRequest};
use serde_json::{json, Value};
pub const FEATURES_OFF: [&str; 14] = [
    "web_search=\"disabled\"",
    "allow_login_shell=false",
    "features.apps=false",
    "features.plugins=false",
    "features.hooks=false",
    "features.multi_agent=false",
    "features.browser_use=false",
    "features.computer_use=false",
    "features.image_generation=false",
    "features.shell_snapshot=false",
    "features.memories=false",
    "features.skill_search=false",
    "features.skip_host_skill_discovery=true",
    "features.code_mode=false",
];
pub fn command(spec: &WorkerSpec, disabled_mcp: &[String]) -> Vec<String> {
    let mut settings = FEATURES_OFF.map(str::to_owned).to_vec();
    settings.push("features.code_mode_host=true".into());
    settings.push(format!(
        "sandbox_workspace_write.network_access={}",
        !spec.workspace.policy.network.is_empty()
    ));
    if spec.workspace.policy.approvals == "never" {
        settings.push("features.request_permissions_tool=false".into());
    }
    if !spec.reasoning_effort.is_empty() {
        settings.push(format!(
            "model_reasoning_effort={}",
            json!(spec.reasoning_effort)
        ));
    }
    for name in disabled_mcp {
        settings.push(format!("mcp_servers.{}.enabled=false", json!(name)));
    }
    let mut words = vec!["codex".into(), "app-server".into()];
    for setting in settings {
        words.extend(["-c".into(), setting]);
    }
    words
}
pub fn sandbox_mode(spec: &WorkerSpec) -> &'static str {
    if spec.confined() || spec.workspace.policy.mode == "full" {
        "danger-full-access"
    } else if spec.workspace.policy.mode == "read-only" {
        "read-only"
    } else {
        "workspace-write"
    }
}
pub fn sandbox_policy(spec: &WorkerSpec) -> Value {
    match sandbox_mode(spec) {
        "danger-full-access" => json!({"type":"dangerFullAccess"}),
        "read-only" => {
            json!({"type":"readOnly","networkAccess":!spec.workspace.policy.network.is_empty()})
        }
        _ => {
            json!({"type":"workspaceWrite","networkAccess":!spec.workspace.policy.network.is_empty(),"writableRoots":[]})
        }
    }
}
fn cwd(spec: &WorkerSpec, params: &mut Value) {
    if !spec.workspace.path.starts_with("~") {
        params["cwd"] = json!(spec.workspace.path);
    }
}
pub(super) async fn handshake(s: &mut Session) -> Result<(), WireError> {
    s.request("initialize",json!({"clientInfo":{"name":"fridica","title":"Fridica","version":"2"},"capabilities":{"experimentalApi":false}})).await?;
    s.send(json!({"method":"initialized","params":{}})).await?;
    if !s.spec.workspace.policy.fetch_repos.is_empty() {
        let config = s.request("config/read", json!({})).await?;
        if !config["config"].is_object()
            || ["mcp_servers", "mcpServers"].iter().any(|key| {
                config["config"][key]
                    .as_object()
                    .map_or(!config["config"][key].is_null(), |servers| {
                        servers.values().any(|v| v["enabled"] != false)
                    })
            })
        {
            return Err(WireError::refusal(
                "codex_mcp_incompatible_with_scoped_fetch",
            ));
        }
    }
    let mut params =
        json!({"sandbox":sandbox_mode(&s.spec),"developerInstructions":s.spec.instructions});
    cwd(&s.spec, &mut params);
    if s.spec.workspace.policy.approvals == "auto" {
        params["approvalPolicy"] = json!("on-request");
        params["approvalsReviewer"] = json!("auto_review");
    } else {
        params["approvalPolicy"] = json!(s.spec.workspace.policy.approvals);
    }
    if !s.spec.model.is_empty() {
        params["model"] = json!(s.spec.model);
    }
    let mut thread = Value::Null;
    if !s.resume.is_empty() {
        let mut resume = params.clone();
        resume["threadId"] = json!(s.resume);
        thread = s.request("thread/resume", resume).await?["thread"].clone();
    }
    if !thread["id"].is_string() {
        params["ephemeral"] = json!(false);
        thread = s.request("thread/start", params).await?["thread"].clone();
    }
    s.session = thread["id"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| WireError::refusal("codex_missing_thread"))?
        .into();
    Ok(())
}
pub(super) async fn job(s: &mut Session, prompt: &str) -> Result<String, WireError> {
    let mut params = json!({"threadId":s.session,"input":[{"type":"text","text":prompt}],"sandboxPolicy":sandbox_policy(&s.spec),"outputSchema":result::schema()});
    cwd(&s.spec, &mut params);
    let start = s.request("turn/start", params).await?;
    s.turn = start["turn"]["id"].as_str().unwrap_or("").into();
    if s.interrupted {
        s.send_interrupt().await?;
    }
    let mut report = None;
    loop {
        let m = s.receive(false).await?;
        if m.get("id").is_some() {
            dispatch(s, m).await?;
            continue;
        }
        let p = &m["params"];
        if p["threadId"].as_str().is_some_and(|id| id != s.session) {
            continue;
        }
        if !s.turn.is_empty() && p["turnId"] != s.turn && p["turn"]["id"] != s.turn {
            continue;
        }
        match m["method"].as_str() {
            Some("item/completed") if p["item"]["type"] == "agentMessage" => {
                if let Some(text) = p["item"]["text"].as_str() {
                    report = Some(text.to_owned());
                }
            }
            Some("turn/completed") => {
                if p["turn"]["status"] == "interrupted" {
                    return Err(WireError::interrupted());
                }
                if p["turn"]["status"] != "completed" {
                    return Err(turn_error(&p["turn"]["error"]));
                }
                if report.is_none() {
                    if let Some(items) = p["turn"]["items"].as_array() {
                        for item in items {
                            if item["type"] == "agentMessage" {
                                report = item["text"].as_str().map(str::to_owned);
                            }
                        }
                    }
                }
                s.turn.clear();
                return Ok(report.unwrap_or_default());
            }
            Some("error") if p["willRetry"] == false => return Err(turn_error(&p["error"])),
            _ => {}
        }
    }
}
fn turn_error(error: &Value) -> WireError {
    if error["message"] == "model refused" {
        WireError::refusal("backend_refused")
    } else {
        WireError::execution("codex_turn_failed")
    }
}
pub(super) async fn dispatch(s: &mut Session, m: Value) -> Result<(), WireError> {
    let Some(method) = m["method"].as_str() else {
        return Ok(());
    };
    let Some(id) = m.get("id") else {
        return Ok(());
    };
    let kind = match method {
        "item/commandExecution/requestApproval" | "execCommandApproval" => "command",
        "item/fileChange/requestApproval" | "applyPatchApproval" => "file_change",
        "item/permissions/requestApproval" => "permissions",
        _ => {
            return s
                .send(json!({"id":id,"error":{"code":-32601,"message":"not supported by Fridica"}}))
                .await;
        }
    };
    let p = if m["params"].is_object() {
        m["params"].clone()
    } else {
        json!({})
    };
    let request_id = id
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| id.to_string());
    let decision = s.approve(describe(kind, &p, &request_id)).await?;
    s.send(json!({"id":id,"result":answer(method,kind,&p,decision)}))
        .await
}
pub fn describe(kind: &str, p: &Value, id: &str) -> ApprovalRequest {
    let (summary, detail, cache) = match kind {
        "command" => {
            let command = if let Some(words) = p["command"].as_array() {
                words
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                p["command"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("a command")
                    .into()
            };
            (
                format!("Run `{}`", command.chars().take(300).collect::<String>()),
                json!({"command":command,"cwd":p["cwd"],"reason":p["reason"]}),
                format!("command:{command}"),
            )
        }
        "file_change" => {
            let root = p["grantRoot"].as_str().unwrap_or("");
            (
                if root.is_empty() {
                    "Apply file changes".into()
                } else {
                    format!("Write files under {root}")
                },
                json!({"reason":p["reason"],"grant_root":p["grantRoot"],"changes":p["changes"]}),
                format!("files:{root}"),
            )
        }
        _ => (
            format!(
                "Grant extra permissions: {}",
                spaced_json(&p["permissions"])
                    .chars()
                    .take(300)
                    .collect::<String>()
            ),
            json!({"permissions":p["permissions"],"reason":p["reason"]}),
            String::new(),
        ),
    };
    ApprovalRequest {
        kind: kind.into(),
        summary,
        detail,
        backend_request_id: id.into(),
        cache_key: cache,
    }
}
pub fn answer(method: &str, kind: &str, p: &Value, decision: ApprovalDecision) -> Value {
    use ApprovalDecision::*;
    if kind == "permissions" {
        return if decision == Deny {
            json!({"permissions":{}})
        } else {
            json!({"permissions":if p["permissions"].is_null(){json!({})}else{p["permissions"].clone()},"scope":if decision==Session{"session"}else{"turn"}})
        };
    }
    let choice = if ["execCommandApproval", "applyPatchApproval"].contains(&method) {
        match decision {
            Once => "approved",
            Session => "approved_for_session",
            Deny => "denied",
        }
    } else {
        match decision {
            Once => "accept",
            Session => "acceptForSession",
            Deny => "decline",
        }
    };
    json!({"decision":choice})
}

// Python's default JSON separators are part of the approval summary text.
fn spaced_json(value: &Value) -> String {
    match value {
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(k, v)| format!("{}: {}", spaced_json(&json!(k)), spaced_json(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(spaced_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => value
            .to_string()
            .chars()
            .map(|c| {
                if c.is_ascii() {
                    c.to_string()
                } else {
                    let mut units = [0; 2];
                    c.encode_utf16(&mut units)
                        .iter()
                        .map(|n| format!("\\u{n:04x}"))
                        .collect::<String>()
                }
            })
            .collect(),
    }
}
