//! Frozen Claude stream-json/control envelopes.
use super::{
    jsonl::{Session, WireError},
    protocol::WorkerSpec,
};
use crate::core::worker::{ApprovalDecision, ApprovalRequest};
use serde_json::{json, Value};
fn expected_mode(spec: &WorkerSpec) -> &'static str {
    let p = &spec.workspace.policy;
    if p.approvals == "auto" {
        "auto"
    } else if p.mode == "read-only" {
        "default"
    } else if p.mode == "full" && p.approvals == "never" {
        "bypassPermissions"
    } else if p.approvals == "untrusted" {
        "default"
    } else {
        "acceptEdits"
    }
}
pub fn settings(spec: &WorkerSpec) -> Value {
    let p = &spec.workspace.policy;
    let sandbox = if spec.confined() || p.mode == "full" {
        json!({"enabled":false})
    } else {
        json!({"enabled":true,"failIfUnavailable":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":p.approvals!="never","excludedCommands":[],"network":{"allowedDomains":p.network,"allowLocalBinding":false}})
    };
    json!({"disableAllHooks":true,"disableClaudeAiConnectors":true,"enabledPlugins":{},"autoMemoryEnabled":false,"sandbox":sandbox})
}
pub fn command(spec: &WorkerSpec, resume: &str, session: &str) -> Vec<String> {
    let p = &spec.workspace.policy;
    let mut allowed = "Read,Glob,Grep";
    let mut mode = "default";
    let tools = if p.mode == "read-only" {
        "Read,Glob,Grep"
    } else {
        if spec.confined() {
            allowed = "Bash,Read,Glob,Grep";
        }
        mode = if p.approvals == "untrusted" {
            "default"
        } else {
            "acceptEdits"
        };
        if p.mode == "full" && p.approvals == "never" {
            mode = "bypassPermissions";
        }
        "Bash,Read,Glob,Grep,Edit,Write"
    };
    if p.approvals == "auto" {
        mode = "auto";
    }
    let mut args = vec![
        "claude".into(),
        "-p".into(),
        "--input-format".into(),
        "stream-json".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--setting-sources".into(),
        "".into(),
        "--settings".into(),
        settings(spec).to_string(),
        "--strict-mcp-config".into(),
        "--mcp-config".into(),
        "{\"mcpServers\":{}}".into(),
        "--disable-slash-commands".into(),
        "--no-chrome".into(),
        "--permission-mode".into(),
        mode.into(),
        "--tools".into(),
        tools.into(),
        "--allowedTools".into(),
        allowed.into(),
        "--append-system-prompt".into(),
        spec.instructions.clone(),
    ];
    if p.approvals == "never" || p.claude_prompts == "none" {
        args.extend(["--permission-prompts".into(), "none".into()]);
    } else {
        args.extend(["--permission-prompt-tool".into(), "stdio".into()]);
    }
    args.extend(if resume.is_empty() {
        ["--session-id".into(), session.into()]
    } else {
        ["--resume".into(), resume.into()]
    });
    if !spec.model.is_empty() {
        args.extend(["--model".into(), spec.model.clone()]);
    }
    if !spec.reasoning_effort.is_empty() {
        args.extend(["--effort".into(), spec.reasoning_effort.clone()]);
    }
    args
}
pub(super) async fn handshake(s: &mut Session) -> Result<(), WireError> {
    let id = s.next_id();
    s.send(json!({"type":"control_request","request_id":format!("fridica-{id}"),"request":{"subtype":"initialize","hooks":null}})).await
}
pub(super) async fn job(s: &mut Session, prompt: &str) -> Result<String, WireError> {
    s.send(
        json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":prompt}]}}),
    )
    .await?;
    s.in_turn = true;
    if s.interrupted {
        s.send_interrupt().await?;
    }
    loop {
        let m = s.receive(false).await?;
        let kind = m["type"].as_str().unwrap_or("");
        if ["system", "result"].contains(&kind) {
            if let Some(id) = m["session_id"].as_str() {
                s.session = id.into();
            }
        }
        if kind == "system" && m["subtype"] == "init" {
            if let Some(actual) = m["permissionMode"].as_str().filter(|s| !s.is_empty()) {
                let expected = expected_mode(&s.spec);
                if actual != expected {
                    s.notice(json!({"code":"claude_permission_mode_fallback","expected":expected,"actual":actual.chars().take(64).collect::<String>()})).await?;
                }
            }
        }
        match kind {
            "control_request" => control(s, m).await?,
            "result" => {
                s.in_turn = false;
                if m["is_error"].as_bool() == Some(true) {
                    return Err(if s.interrupted {
                        WireError::interrupted()
                    } else {
                        WireError::execution("claude_job_failed")
                    });
                }
                return Ok(m["result"].as_str().unwrap_or("").into());
            }
            _ => {}
        }
    }
}
async fn control(s: &mut Session, m: Value) -> Result<(), WireError> {
    let r = &m["request"];
    let id = &m["request_id"];
    if r["subtype"] != "can_use_tool" {
        return s.send(json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"not supported by Fridica"}})).await;
    }
    let tool = r["tool_name"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or("tool");
    let input = if r["input"].is_object() {
        r["input"].clone()
    } else {
        json!({})
    };
    let target = ["command", "file_path", "path"]
        .into_iter()
        .find_map(|k| input[k].as_str().filter(|s| !s.is_empty()))
        .unwrap_or("");
    let request = ApprovalRequest {
        kind: if tool == "Bash" { "command" } else { "tool" }.into(),
        summary: if target.is_empty() {
            format!("Use {tool}")
        } else {
            format!("{tool}: {}", target.chars().take(300).collect::<String>())
        },
        detail: json!({"tool":tool,"input":input,"reason":r.get("description").filter(|v|!v.is_null()).unwrap_or(&r["decision_reason"])}),
        backend_request_id: id
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| id.to_string()),
        cache_key: if target.is_empty() {
            String::new()
        } else {
            format!("{tool}:{target}")
        },
    };
    let decision = s.approve(request).await?;
    let response = if decision == ApprovalDecision::Deny {
        json!({"behavior":"deny","message":"The owner declined this action; continue without it."})
    } else {
        json!({"behavior":"allow","updatedInput":input})
    };
    s.send(json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":response}})).await
}
