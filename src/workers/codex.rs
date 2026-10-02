//! Codex app-server driver settings for a worker; the protocol lives in
//! `fridica_agent::codex`.
use super::{jsonl::driver_spec, protocol::WorkerSpec};
use crate::core::worker::{ApprovalDecision, ApprovalRequest};
pub use fridica_agent::codex::FEATURES_OFF;
use fridica_agent::Backend;
use serde_json::Value;
/// The startup helper disables the target's MCP servers; none is named here.
pub fn command(spec: &WorkerSpec) -> Vec<String> {
    fridica_agent::codex::command(&driver_spec(spec, Backend::Codex), &[])
}
pub fn sandbox_mode(spec: &WorkerSpec) -> &'static str {
    fridica_agent::codex::sandbox_mode(&driver_spec(spec, Backend::Codex))
}
pub fn sandbox_policy(spec: &WorkerSpec) -> Value {
    fridica_agent::codex::sandbox_policy(&driver_spec(spec, Backend::Codex))
}
pub fn describe(kind: &str, p: &Value, id: &str) -> ApprovalRequest {
    super::jsonl::approval_request(fridica_agent::codex::describe(kind, p, id))
}
pub fn answer(method: &str, kind: &str, p: &Value, decision: ApprovalDecision) -> Value {
    fridica_agent::codex::answer(method, kind, p, super::jsonl::agent_decision(decision))
}
