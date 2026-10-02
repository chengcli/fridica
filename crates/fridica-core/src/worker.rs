//! Backend-independent worker records and complete structured results.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub path: String,
    #[serde(default = "modified")]
    pub change: String,
    #[serde(default)]
    pub note: String,
}
fn modified() -> String {
    "modified".into()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Validation {
    pub command: String,
    #[serde(default = "passed")]
    pub outcome: String,
    #[serde(default)]
    pub detail: String,
}
fn passed() -> String {
    "passed".into()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRef {
    pub path: String,
    #[serde(default = "markdown")]
    pub kind: String,
    #[serde(default)]
    pub caption: String,
}
fn markdown() -> String {
    "md".into()
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MachineState {
    pub branch: String,
    pub commit: String,
    pub dirty: bool,
    pub notes: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkerResult {
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub changes: Vec<Change>,
    #[serde(default)]
    pub validation: Vec<Validation>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactRef>,
    #[serde(default)]
    pub machine_state: MachineState,
    #[serde(default)]
    pub unresolved: Vec<String>,
    #[serde(default)]
    pub question: String,
    #[serde(default)]
    pub report: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerRecord {
    pub id: String,
    pub session_id: String,
    pub machine: String,
    pub workspace: String,
    pub backend: String,
    #[serde(default = "general")]
    pub role: String,
    #[serde(default)]
    pub ephemeral: bool,
    #[serde(default)]
    pub backend_session_id: String,
    #[serde(default = "idle")]
    pub status: String,
    #[serde(default)]
    pub slot: usize,
    #[serde(default)]
    pub updated: f64,
}
fn general() -> String {
    "general".into()
}
fn idle() -> String {
    "idle".into()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    pub worker_id: String,
    pub session_id: String,
    pub brief: String,
    #[serde(default)]
    pub join_group: String,
    #[serde(default)]
    pub inbox_id: Option<i64>,
    #[serde(default = "report")]
    pub deliverable: String,
    #[serde(default)]
    pub fetch_repo: String,
    #[serde(default)]
    pub fetch_ref: String,
    /// Slack file IDs placed in the workspace before the job starts.
    #[serde(default)]
    pub files: Vec<String>,
    /// How the worker's context starts: forked from the delegating turn, or fresh.
    #[serde(default)]
    pub context: crate::fork::ContextMode,
    /// The thread's context at the fork point; `Some` for a fork of either kind.
    #[serde(default)]
    pub snapshot: Option<crate::fork::ContextBundle>,
    /// For `fork_worker`: the worker whose backend session this job forks;
    /// its session is read at start and never changed.
    #[serde(default)]
    pub fork_from_worker: String,
    #[serde(default = "queued")]
    pub status: String,
    #[serde(default)]
    pub attempt: u32,
    #[serde(default)]
    pub work_item_id: String,
    #[serde(default)]
    pub target_sha: String,
    #[serde(default)]
    pub target_tree: String,
    #[serde(default)]
    pub retry_of: String,
    #[serde(default = "worker_clearance")]
    pub clearance: String,
}
fn worker_clearance() -> String {
    "worker".into()
}
fn report() -> String {
    "report".into()
}
fn queued() -> String {
    "queued".into()
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Execution,
    Refusal,
    Cancelled,
    Interrupted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerFailure {
    pub kind: Failure,
    pub code: String,
    #[serde(default)]
    pub backend_session_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub result: WorkerResult,
    pub backend_session_id: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub kind: String,
    pub summary: String,
    #[serde(default)]
    pub detail: Value,
    #[serde(default)]
    pub backend_request_id: String,
    #[serde(default)]
    pub cache_key: String,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Once,
    Session,
    Deny,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CollectedArtifact {
    pub reference: ArtifactRef,
    pub data: Option<Vec<u8>>,
    pub error: String,
}
/// Private request contents belong in the state database, never diagnostic logs.
#[derive(Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: String,
    pub worker_id: String,
    pub job_id: String,
    pub session_id: String,
    pub backend_request_id: String,
    pub kind: String,
    pub summary: String,
    pub detail: Value,
    pub status: String,
    pub scope: String,
    pub decided_by: String,
    pub created: f64,
    pub decided_at: f64,
    pub expires_at: f64,
}
pub fn retry_same_session(failure: Failure, attempt: u32, session: &str) -> bool {
    failure == Failure::Execution && attempt == 0 && !session.is_empty()
}
