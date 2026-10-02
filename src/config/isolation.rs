//! Owner-provisioned worker boundaries. No credentials, discovery I/O or jobs.
//! Workers always run without MCP servers, and confinement needs no inventory,
//! so the `[isolation]` table has no settings left; it is accepted empty.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

/// Settings the `[isolation]` table once carried. A configuration that still
/// sets one is refused with a message naming it.
pub const REMOVED: [&str; 5] = [
    "settings_files",
    "mcp_inventory_complete",
    "mcp_aliases",
    "mcp_urls",
    "remote",
];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {}

impl Settings {
    pub fn is_empty(&self) -> bool {
        true
    }
    /// Configuration coverage only. This does not claim target reachability
    /// or runtime isolation support.
    pub fn summary(&self) -> Value {
        json!({"runtime_checks":"not_run"})
    }
}

pub fn remote_path(value: &str) -> bool {
    let expanded = value
        .strip_prefix("~/")
        .map(|tail| format!("/remote-home/{tail}"));
    let path = Path::new(expanded.as_deref().unwrap_or(value));
    path.is_absolute()
        && path != Path::new("/")
        && !value.ends_with('/')
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && !value.split('/').any(|part| part == "." || part == "..")
}
