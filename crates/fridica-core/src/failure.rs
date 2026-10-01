//! Safe settlement for unavailable or invalid model answers. Infrastructure and
//! recording faults still propagate; no unrecorded effect is acknowledged.
use crate::parent::{Decision, ParentFailure};
use serde_json::json;
pub const UNAVAILABLE: &str =
    "I couldn't get to this just now; I'll need to look at it myself before anyone retries.";
pub const INVALID: &str = "I couldn't produce a valid action after one repair attempt. Owner review is needed before retrying.";
pub fn blocked(invalid: bool) -> Decision {
    let text = if invalid { INVALID } else { UNAVAILABLE };
    serde_json::from_value(json!({"reply":{"text":text,"status":"blocked"},
        "note":{"kind":"status","blocker":text,"next_step":"Owner review before retrying"}}))
    .expect("fixed failure action")
}
pub fn prevents_effects(failure: &ParentFailure) -> bool {
    matches!(
        failure.code.as_str(),
        "parent_recording_failed"
            | "parent_context_recording_failed"
            | "parent_context_snapshot_missing"
            | "parent_context_snapshot_invalid"
            | "parent_context_scope"
            | "parent_context_time"
    )
}
