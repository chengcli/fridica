//! Claude stream-json driver settings for a worker; the protocol lives in
//! `fridica_agent::claude`.
use super::{jsonl::driver_spec, protocol::WorkerSpec};
use fridica_agent::Backend;
use serde_json::Value;
pub fn settings(spec: &WorkerSpec) -> Value {
    fridica_agent::claude::settings(&driver_spec(spec, Backend::Claude))
}
pub fn command(spec: &WorkerSpec, resume: &str, session: &str, fork_from: &str) -> Vec<String> {
    fridica_agent::claude::command(
        &driver_spec(spec, Backend::Claude),
        resume,
        session,
        fork_from,
    )
}
