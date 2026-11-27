//! Owner-configured command prefixes and exact MCP tool selectors.
use crate::{
    config::registry::Policy,
    core::worker::{ApprovalDecision, ApprovalRequest, Job},
};
use serde_json::{json, Value};

/// Preserve command rules without treating MCP selectors as shell prefixes.
pub fn decide(policy: &Policy, request: &ApprovalRequest) -> Option<ApprovalDecision> {
    let mut commands = policy.clone();
    commands.auto_approve.retain(|r| !r.starts_with("mcp:"));
    commands.auto_deny.retain(|r| !r.starts_with("mcp:"));
    fridica_core::approvals::decide(&commands, request)
}

pub(super) fn for_job(
    policy: &Policy,
    backend: &str,
    job: &Job,
    request: &ApprovalRequest,
) -> Option<ApprovalDecision> {
    if request.kind != "tool" {
        return decide(policy, request);
    }
    if policy
        .auto_deny
        .iter()
        .filter_map(|r| Selector::parse(r))
        .any(|selector| selector.params(request, backend).is_some())
    {
        return Some(ApprovalDecision::Deny);
    }
    if policy.mode == "read-only" {
        return None;
    }
    policy.auto_approve.iter().find_map(|r| {
        let selector = Selector::parse(r)?;
        let params = selector.params(request, backend)?;
        if selector.server == "overleaf" {
            let project = selector.project?;
            if crate::overleaf::worker::job_project(&job.brief) != Some(project) {
                return None;
            }
            if matches!(
                selector.tool,
                "native_add_comment" | "native_reply_comment" | "native_suggest_edit"
            ) && !params["operation_id"].as_str().is_some_and(|id| {
                uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id)
            }) {
                return None;
            }
            if selector.tool == "native_suggest_edit"
                && (params.get("track").is_some_and(|v| v != "on")
                    || params.get("strict_version").is_some_and(|v| v != true)
                    || params["expected_version"].as_u64().is_none())
            {
                return None;
            }
        }
        Some(ApprovalDecision::Once)
    })
}

struct Selector<'a> {
    server: &'a str,
    tool: &'a str,
    project: Option<&'a str>,
}
impl<'a> Selector<'a> {
    fn parse(rule: &'a str) -> Option<Self> {
        let (server, rest) = rule.strip_prefix("mcp:")?.split_once('/')?;
        let (tool, project) = rest
            .split_once('@')
            .map_or((rest, None), |(tool, project)| (tool, Some(project)));
        let name = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };
        if !name(server)
            || !name(tool)
            || project
                .is_some_and(|id| server != "overleaf" || !crate::config::overleaf::valid_id(id))
        {
            return None;
        }
        Some(Self {
            server,
            tool,
            project,
        })
    }

    fn params<'r>(&self, request: &'r ApprovalRequest, backend: &str) -> Option<&'r Value> {
        let detail = &request.detail;
        let claude = format!("mcp__{}__{}", self.server, self.tool);
        let params = match backend {
            "claude" => {
                if detail["tool"] != claude
                    || ["serverName", "message", "mode", "requestedSchema", "_meta"]
                        .iter()
                        .any(|key| detail.get(key).is_some())
                {
                    return None;
                }
                &detail["input"]
            }
            "codex" => {
                let message = format!(
                    "Allow the {} MCP server to run tool \"{}\"?",
                    self.server, self.tool
                );
                if detail.get("tool").is_some()
                    || detail.get("input").is_some()
                    || detail["serverName"] != self.server
                    || detail["message"] != message
                    || detail["mode"] != "form"
                    || detail["_meta"]["codex_approval_kind"] != "mcp_tool_call"
                    || detail["requestedSchema"] != json!({"type":"object","properties":{}})
                {
                    return None;
                }
                &detail["_meta"]["tool_params"]
            }
            _ => return None,
        };
        if !params.is_object() {
            return None;
        }
        if let Some(project) = self.project {
            let target = match (params.get("project_id"), params.get("project")) {
                (Some(id), None) => id.as_str() == Some(project),
                (None, Some(url)) => {
                    url.as_str() == Some(&format!("https://www.overleaf.com/project/{project}"))
                }
                _ => false,
            };
            if !target {
                return None;
            }
        }
        Some(params)
    }
}
