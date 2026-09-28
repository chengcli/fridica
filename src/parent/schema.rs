//! Strict structured-output schemas for the implemented parent action surface.
use serde_json::{json, Value};
fn object(properties: Value) -> Value {
    let required: Vec<_> = properties.as_object().unwrap().keys().cloned().collect();
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn string() -> Value {
    json!({"type":"string"})
}
fn strings() -> Value {
    json!({"type":"array","items":string()})
}
pub fn triage() -> Value {
    object(json!({"decision":{"type":"string","enum":["ignore","observe","respond"]}}))
}
pub fn decision() -> Value {
    let reply = object(
        json!({"text":string(),"details":string(),"status":{"type":"string","enum":["complete","waiting","blocked"]},"answers":strings()}),
    );
    let delegation = object(
        json!({"brief":string(),"worker_id":string(),"machine":string(),"workspace":string(),"backend":string(),"tags":strings(),"role":{"type":"string","enum":["general","implementer","reviewer","tester"]},"ephemeral":{"type":"boolean"},"deliverable":{"type":"string","enum":["report","markdown","figures_pdf"]},"fetch_repo":string(),"fetch_ref":string()}),
    );
    let declined = object(
        json!({"state":{"type":"string","enum":["declined"]},"id":string(),"reason":string()}),
    );
    let deferred = object(
        json!({"state":{"type":"string","enum":["deferred"]},"id":string(),"reason":string(),"until":{"type":"number"}}),
    );
    object(
        json!({"reply":{"anyOf":[reply,{"type":"null"}]},"delegations":{"type":"array","items":delegation},"summary":string(),
        "dispositions":{"type":"array","items":{"anyOf":[declined,deferred]}},"asks":{"type":"array","items":object(json!({"summary":string(),"due":{"type":"number"}}))},"reopen_blocked":{"type":"boolean"}}),
    )
}
