//! Codex app-server driver settings for a worker; the protocol lives in
//! `fridica_agent::codex`.
use super::{jsonl::driver_spec, protocol::WorkerSpec};
pub use fridica_agent::codex::{answer, describe, FEATURES_OFF};
use fridica_agent::Backend;
use serde_json::Value;
pub fn command(spec: &WorkerSpec, disabled_mcp: &[String]) -> Vec<String> {
    fridica_agent::codex::command(&driver_spec(spec, Backend::Codex), disabled_mcp)
}
pub fn sandbox_mode(spec: &WorkerSpec) -> &'static str {
    fridica_agent::codex::sandbox_mode(&driver_spec(spec, Backend::Codex))
}
pub fn sandbox_policy(spec: &WorkerSpec) -> Value {
    fridica_agent::codex::sandbox_policy(&driver_spec(spec, Backend::Codex))
}
