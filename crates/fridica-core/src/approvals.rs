//! Automatic command rules do not parse or execute shell input.
use crate::{
    config::registry::Policy,
    worker::{ApprovalDecision, ApprovalRequest},
};
fn matches(command: &str, prefix: &str) -> bool {
    command == prefix
        || command
            .strip_prefix(prefix)
            .is_some_and(|tail| tail.starts_with(' '))
}
pub fn decide(policy: &Policy, request: &ApprovalRequest) -> Option<ApprovalDecision> {
    if request.kind != "command" {
        return None;
    }
    let command = request
        .detail
        .get("command")
        .filter(|v| !v.is_null())
        .or_else(|| request.detail.get("input")?.get("command"))?
        .as_str()?
        .trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    if command.is_empty() {
        return None;
    }
    if policy.auto_deny.iter().any(|p| matches(command, p)) {
        return Some(ApprovalDecision::Deny);
    }
    if command.contains([
        ';', '&', '|', '`', '$', '<', '>', '(', ')', '{', '}', '\n', '\\',
    ]) {
        return None;
    }
    policy
        .auto_approve
        .iter()
        .any(|p| matches(command, p))
        .then_some(ApprovalDecision::Once)
}
