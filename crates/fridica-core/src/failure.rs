//! Safe settlement for unavailable or invalid model answers. Infrastructure and
//! recording faults still propagate; no unrecorded effect is acknowledged.
use crate::parent::{Decision, ParentFailure};
use serde_json::json;
pub const UNAVAILABLE: &str =
    "I couldn't get to this just now; I'll need to look at it myself before anyone retries.";
pub const INVALID: &str = "I couldn't produce a valid action after one repair attempt. Owner review is needed before retrying.";
/// The turn's stand-in decision when the parent fails or its action stays
/// invalid after repair. Nothing is posted: the thread waits for owner review.
pub fn blocked(invalid: bool) -> Decision {
    let text = if invalid { INVALID } else { UNAVAILABLE };
    // Neither failure is posted (send: false): the thread is still marked
    // blocked, so due events do not call the model again, the requester's
    // mention stays open, and the host escalates the cause to the owner.
    let reply = json!({"send":false,"text":text,"status":"blocked"});
    serde_json::from_value(json!({"reply":reply,
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
