//! Stateless, tool-less CLI calls using the owner's existing CLI authentication.
//! Never fall back to a less restrictive command when a flag is unsupported.
use crate::exec::process::{self, Launch};
use serde_json::{json, Value};
use std::{collections::BTreeMap, ffi::OsString, path::Path, time::Duration};
pub const FEATURES_OFF: [&str; 22] = [
    "approval_policy=\"never\"",
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
    "features.code_mode_host=false",
    "features.request_permissions_tool=false",
    "features.shell_tool=false",
    "features.unified_exec=false",
    "features.view_image=false",
    "project_doc_max_bytes=0",
    "sandbox_workspace_write.network_access=false",
];
fn failure(code: &str) -> crate::core::parent::ParentFailure {
    crate::core::parent::ParentFailure { code: code.into() }
}
pub fn command(
    backend: &str,
    program: &str,
    directory: &Path,
    schema: &Value,
    model: &str,
    effort: &str,
) -> Result<Vec<String>, crate::core::parent::ParentFailure> {
    let mut argv:Vec<String>=match backend {
        "claude"=>vec![program.into(),"-p".into(),"--output-format".into(),"json".into(),"--json-schema".into(),schema.to_string(),
            "--setting-sources".into(),"".into(),"--settings".into(),json!({"disableAllHooks":true,"disableClaudeAiConnectors":true,"enabledPlugins":{},"autoMemoryEnabled":false}).to_string(),
            "--strict-mcp-config".into(),"--mcp-config".into(),"{\"mcpServers\":{}}".into(),"--disable-slash-commands".into(),"--no-chrome".into(),
            "--permission-mode".into(),"dontAsk".into(),"--tools".into(),"".into(),"--no-session-persistence".into()],
        "codex"=>vec![program.into(),"exec".into(),"--ignore-user-config".into(),"--ignore-rules".into(),"--ephemeral".into(),"--skip-git-repo-check".into(),"--sandbox".into(),"read-only".into(),"--output-schema".into(),directory.join("schema.json").to_string_lossy().into_owned(),"--json".into(),"--color".into(),"never".into()],
        _=>return Err(failure("parent_unknown_backend")),
    };
    if backend == "codex" {
        for setting in FEATURES_OFF {
            argv.extend(["-c".into(), setting.into()]);
        }
    }
    if backend == "claude" && !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    if !effort.is_empty() {
        if backend == "codex" {
            argv.extend([
                "-c".into(),
                format!("model_reasoning_effort={}", json!(effort)),
            ]);
        } else {
            argv.extend(["--effort".into(), effort.into()]);
        }
    }
    if backend == "codex" && !model.is_empty() {
        argv.extend(["--model".into(), model.into()]);
    }
    if backend == "codex" {
        argv.push("-".into());
    }
    Ok(argv)
}
/// Whether Claude's result envelope reports a usage limit (HTTP 429); the
/// CLI then exits non-zero with the reason only in this envelope.
pub fn rate_limited(backend: &str, output: &[u8]) -> bool {
    backend == "claude"
        && serde_json::from_slice::<Value>(output).is_ok_and(|envelope| {
            envelope["is_error"] == true
                && (envelope["api_error_status"] == 429 || envelope["error"] == "rate_limit")
        })
}
/// Whether Claude failed because another process was refreshing the same
/// expired OAuth login; the backend calls this transient.
pub fn auth_contended(backend: &str, output: &[u8]) -> bool {
    backend == "claude"
        && serde_json::from_slice::<Value>(output).is_ok_and(|envelope| {
            envelope["is_error"] == true
                && envelope["result"]
                    .as_str()
                    .is_some_and(|r| r.starts_with("Failed to refresh OAuth token"))
        })
}
pub fn parse(backend: &str, output: &[u8]) -> Result<Value, crate::core::parent::ParentFailure> {
    let output = std::str::from_utf8(output).map_err(|_| failure("parent_invalid_utf8"))?;
    if backend == "claude" {
        let envelope: Value =
            serde_json::from_str(output).map_err(|_| failure("parent_invalid_json"))?;
        if !envelope.is_object() || envelope["is_error"].as_bool().unwrap_or(true) {
            return Err(failure("parent_backend_error"));
        }
        if envelope
            .get("permission_denials")
            .is_some_and(|v| v.as_array().is_none_or(|a| !a.is_empty()))
        {
            return Err(failure("parent_tools_attempted"));
        }
        return envelope
            .get("structured_output")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or_else(|| failure("parent_missing_structured_output"));
    }
    if backend != "codex" {
        return Err(failure("parent_unknown_backend"));
    }
    let mut text = None;
    let (mut started, mut completed, mut diagnostics) = (false, false, false);
    for line in output.lines().filter(|s| !s.trim().is_empty()) {
        // Unlike the frozen parser, malformed lines cannot hide tool events.
        let event: Value =
            serde_json::from_str(line).map_err(|_| failure("parent_invalid_json"))?;
        if !event.is_object() {
            return Err(failure("parent_invalid_envelope"));
        }
        if !matches!(
            event["type"].as_str(),
            Some(
                "thread.started"
                    | "turn.started"
                    | "turn.completed"
                    | "turn.failed"
                    | "error"
                    | "item.started"
                    | "item.updated"
                    | "item.completed"
            )
        ) {
            return Err(failure("parent_invalid_envelope"));
        }
        if matches!(event["type"].as_str(), Some("error" | "turn.failed")) {
            return Err(failure("parent_backend_error"));
        }
        match event["type"].as_str() {
            Some("turn.started") => started = true,
            Some("turn.completed") => completed = true,
            _ => {}
        }
        if let Some(item) = event.get("item") {
            if !item.is_object()
                || !matches!(
                    item["type"].as_str(),
                    Some("reasoning" | "agent_message" | "error")
                )
            {
                return Err(failure("parent_tools_attempted"));
            }
            // Codex can report startup diagnostics as error items before the
            // turn starts; those are tolerated only if the turn then completes.
            // An error item inside the turn is fatal.
            if item["type"] == "error" {
                if started {
                    return Err(failure("parent_backend_error"));
                }
                diagnostics = true;
            }
            if event["type"] == "item.completed" && item["type"] == "agent_message" {
                text = item["text"].as_str().map(str::to_owned);
            }
        }
    }
    if diagnostics && !completed {
        return Err(failure("parent_backend_error"));
    }
    let text = text.ok_or_else(|| failure("parent_missing_structured_output"))?;
    let result: Value =
        serde_json::from_str(&text).map_err(|_| failure("parent_invalid_structured_output"))?;
    if !result.is_object() {
        return Err(failure("parent_invalid_structured_output"));
    }
    Ok(result)
}
pub async fn execute(
    argv: Vec<String>,
    cwd: &Path,
    environment: BTreeMap<OsString, OsString>,
    prompt: String,
    timeout: Duration,
) -> Result<process::Completed, crate::core::parent::ParentFailure> {
    process::run_once(
        Launch {
            argv,
            cwd: Some(cwd.into()),
            env: environment,
        },
        prompt.into_bytes(),
        timeout,
        process::OUTPUT_LIMIT,
    )
    .await
    .map_err(|error| {
        failure(match error.to_string().as_str() {
            "process timed out" => "parent_timeout",
            "process output exceeded the size limit" => "parent_output_limit",
            _ => "parent_process_failed",
        })
    })
}
