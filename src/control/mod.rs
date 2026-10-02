//! Authenticated Unix-socket control adapters. Authority never comes from JSON.
pub mod api;
pub mod cli;
pub mod client;
pub mod events;
pub mod server;
pub mod views;
use crate::core::{delivery::AdapterFuture, Authority};
use serde_json::{json, Value};
pub const BODY_LIMIT: usize = 64 * 1024;
pub const RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
#[derive(Clone)]
pub struct Request {
    pub method: String,
    pub target: String,
    pub body: Value,
}
#[derive(Clone)]
pub struct Response {
    pub status: u16,
    pub body: Value,
}
impl Response {
    pub fn ok(body: Value) -> Self {
        Self { status: 200, body }
    }
    pub fn error(status: u16, code: &str) -> Self {
        Self {
            status,
            body: json!({"error":code}),
        }
    }
}
pub trait Backend: Send + Sync + 'static {
    fn request(&self, request: Request, authority: Authority) -> AdapterFuture<'_, Response>;
}
