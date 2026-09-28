use super::delivery::AdapterFuture;
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
    pub previous: Option<Value>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParentFailure {
    pub code: String,
}

pub trait Parent: Send + Sync {
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
    #[serde(default)]
    pub details: String,
    pub text: String,
    pub status: ReplyStatus,
    #[serde(default)]
    pub answers: Vec<String>,
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
    pub delegations: Vec<Delegation>,
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
}
