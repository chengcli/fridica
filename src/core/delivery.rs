//! Delivery adapter boundary. Unknown transport outcomes must be Ambiguous.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{future::Future, pin::Pin};

pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Post {
    pub idem_key: String,
    pub session_id: String,
    pub kind: String,
    pub channel: String,
    pub thread_ts: Option<String>,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub meta: Option<Value>,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub blob: Option<Vec<u8>>,
    #[serde(default)]
    pub after: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClaimedPost {
    pub id: i64,
    pub attempt: u32,
    pub post: Post,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum DeliveryOutcome {
    Sent { reference: String },
    RateLimited { retry_after: f64 },
    Rejected { code: String },
    Ambiguous { code: String },
}

pub trait Delivery: Send + Sync {
    fn send(&self, post: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome>;
}
