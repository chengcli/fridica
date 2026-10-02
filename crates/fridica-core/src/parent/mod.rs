pub mod context;
pub mod schema;
use crate::delivery::AdapterFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParentRequest {
    pub inbox_id: i64,
    pub call: String,
    pub session: Value,
    pub trigger: Value,
    pub history: Vec<Value>,
    pub obligations: Vec<Value>,
    #[serde(default)]
    pub linked: Vec<Value>,
    #[serde(default)]
    pub github_state: Vec<Value>,
    pub previous: Option<Value>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParentFailure {
    pub code: String,
}

pub trait Parent: Send + Sync {
    /// Trusted adapters may reserve a separate bounded context-read allowance.
    /// The actor caps this at 30 seconds; model output cannot choose the budget.
    fn preparation_timeout(&self, _request: &ParentRequest) -> std::time::Duration {
        std::time::Duration::ZERO
    }
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyStatus {
    Complete,
    Waiting,
    Blocked,
}
impl ReplyStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Waiting => "waiting",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default = "send_by_default")]
    pub send: bool,
    #[serde(default)]
    pub discussion: Discussion,
    #[serde(default)]
    pub details: String,
    pub text: String,
    pub status: ReplyStatus,
    #[serde(default)]
    pub answers: Vec<String>,
}
fn send_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discussion {
    #[default]
    Ongoing,
    Finished,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadContext {
    pub machine: String,
    pub workspace: String,
    pub repo: String,
    pub branch: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    #[default]
    Result,
    Question,
    Status,
    Ack,
    Correction,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TaskNote {
    pub kind: NoteKind,
    pub repo: String,
    pub assignee: String,
    pub next_step: String,
    pub blocker: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Debrief {
    pub debrief: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Disposition {
    Declined {
        id: String,
        reason: String,
    },
    Deferred {
        id: String,
        reason: String,
        until: f64,
    },
}
impl Disposition {
    pub fn id(&self) -> &str {
        match self {
            Self::Declined { id, .. } | Self::Deferred { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub summary: String,
    pub due: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    #[serde(default)]
    pub worker_control: Vec<WorkerControl>,
    #[serde(default, alias = "delegate")]
    pub delegations: Vec<Delegation>,
    #[serde(default)]
    pub context: ThreadContext,
    #[serde(default)]
    pub note: TaskNote,
    #[serde(default)]
    pub decisions: Vec<String>,
    #[serde(default)]
    pub reply: Option<Reply>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub dispositions: Vec<Disposition>,
    #[serde(default)]
    pub asks: Vec<Ask>,
    #[serde(default)]
    pub reopen_blocked: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerControl {
    pub worker_id: String,
    pub op: WorkerOperation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerOperation {
    Interrupt,
    Stop,
}

/// A placement request contains configured names only, never execution paths.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delegation {
    pub brief: String,
    pub worker_id: String,
    pub machine: String,
    pub workspace: String,
    pub backend: String,
    pub tags: Vec<String>,
    pub role: String,
    pub ephemeral: bool,
    pub deliverable: String,
    pub fetch_repo: String,
    pub fetch_ref: String,
    /// Files attached in this thread (`session.files[].id`) that Fridica
    /// places in the worker's workspace before the job starts.
    pub files: Vec<String>,
    /// How much of the thread the worker inherits: a fork of this turn's
    /// context (default), only the brief, or a copy of another worker's
    /// backend session (`fork_worker`, with `fork_worker_id`).
    pub context: crate::fork::ContextMode,
    /// With `context: fork_worker`: the live worker of this thread whose
    /// backend session the new worker is forked from.
    pub fork_worker_id: String,
}
