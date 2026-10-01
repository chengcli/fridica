//! Fridica's domain: threads and their control, parent decisions and
//! delegation, workers and their results, machines and placement, delivery
//! outcomes, attention policy and reply rendering. No I/O: storage, Slack and
//! worker processes belong to the host, which calls into these types and
//! functions and implements the adapter traits (`Parent`, `Delivery`).
pub mod approvals;
pub mod config;
pub mod delegation;
pub mod delivery;
pub mod failure;
pub mod ids;
pub mod parent;
pub mod placement;
pub mod policy;
pub mod render;
pub mod result;
pub mod time;
pub mod worker;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Authority {
    Owner,
    Lead { campaign: String },
    Overseer,
    System,
    DesktopReadOnly,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ThreadControl {
    Active,
    Paused {
        by: Authority,
        reason: String,
        since: f64,
    },
    Closed,
    Archived,
    Cleaned,
}

impl ThreadControl {
    pub fn can_resume(&self, actor: &Authority) -> bool {
        match self {
            Self::Paused {
                by: Authority::Owner,
                ..
            } => *actor == Authority::Owner,
            Self::Paused { .. } => matches!(actor, Authority::Owner | Authority::Overseer),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryState {
    Pending,
    Sending,
    Sent,
    Failed,
    Ambiguous,
    Blocked,
}

impl DeliveryState {
    pub fn after_restart(self) -> Self {
        if self == Self::Sending {
            Self::Ambiguous
        } else {
            self
        }
    }
}
