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
pub fn debrief() -> Value {
    object(json!({"debrief":string()}))
}
/// Accepts "" or one of `values`; with no values only "" remains.
fn choice(values: &[String]) -> Value {
    let mut allowed = vec![String::new()];
    allowed.extend(values.iter().filter(|v| !v.is_empty()).cloned());
    allowed.dedup();
    json!({"type":"string","enum":allowed})
}
/// Values the decide/repair schema allows, taken from the same snapshot the
/// parent sees, so identifiers and grants cannot be invented.
#[derive(Default)]
pub struct Choices {
    /// Workers of this thread that can take new work.
    pub delegable: Vec<String>,
    /// Workers of this thread that can be interrupted or stopped.
    pub controllable: Vec<String>,
    /// Live workers of this thread with a backend session a new worker can fork.
    pub forkable: Vec<String>,
    /// Repositories granted in any workspace's fetch_repos.
    pub fetch_repos: Vec<String>,
    /// Tags some machine has, for delegation selectors.
    pub tags: Vec<String>,
    /// Files attached in this thread that a worker may be given.
    pub files: Vec<String>,
}
impl Choices {
    pub fn from_session(session: &Value) -> Self {
        let id = session["id"].as_str().unwrap_or("");
        let mut choices = Self::default();
        for worker in session["work"]["workers"].as_array().into_iter().flatten() {
            let (Some(worker_id), true) = (worker["id"].as_str(), worker["session_id"] == id)
            else {
                continue;
            };
            choices.controllable.push(worker_id.into());
            if worker["status"] != "stopped" {
                choices.delegable.push(worker_id.into());
                if worker["backend_session_id"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
                {
                    choices.forkable.push(worker_id.into());
                }
            }
        }
        for machine in session["machines"].as_array().into_iter().flatten() {
            for repos in machine["fetch_repos"]
                .as_object()
                .into_iter()
                .flat_map(|m| m.values())
            {
                choices.fetch_repos.extend(
                    repos
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| r.as_str())
                        .map(String::from),
                );
            }
        }
        for machine in session["machines"].as_array().into_iter().flatten() {
            choices.tags.extend(
                machine["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str())
                    .map(String::from),
            );
        }
        choices.files = session["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["id"].as_str())
            .map(String::from)
            .collect();
        for values in [&mut choices.fetch_repos, &mut choices.tags] {
            values.sort();
            values.dedup();
        }
        choices
    }
}
pub fn decision(choices: &Choices) -> Value {
    let reply = object(
        json!({"send":{"type":"boolean"},"discussion":{"type":"string","enum":["ongoing","finished"]},"text":string(),"details":string(),"status":{"type":"string","enum":["complete","waiting","blocked"]},"answers":strings()}),
    );
    let delegation = object(
        json!({"brief":string(),"worker_id":choice(&choices.delegable),"machine":string(),"workspace":string(),"backend":string(),"tags":{"type":"array","items":choice(&choices.tags)},"role":{"type":"string","enum":["general","implementer","reviewer","tester"]},"ephemeral":{"type":"boolean"},"deliverable":{"type":"string","enum":["report","markdown","figures_pdf"]},"fetch_repo":choice(&choices.fetch_repos),"fetch_ref":string(),"files":{"type":"array","items":choice(&choices.files)},"context":{"type":"string","enum":["fork","fresh","fork_worker"]},"fork_worker_id":choice(&choices.forkable)}),
    );
    let declined = object(
        json!({"state":{"type":"string","enum":["declined"]},"id":string(),"reason":string()}),
    );
    let deferred = object(
        json!({"state":{"type":"string","enum":["deferred"]},"id":string(),"reason":string(),"until":{"type":"number"}}),
    );
    object(
        json!({"reply":{"anyOf":[reply,{"type":"null"}]},"delegations":{"type":"array","items":delegation},"summary":string(),
        "worker_control":{"type":"array","items":object(json!({"worker_id":choice(&choices.controllable),"op":{"type":"string","enum":["interrupt","stop"]}}))},
        "context":object(json!({"machine":string(),"workspace":string(),"repo":string(),"branch":string()})),
        "note":object(json!({"kind":{"type":"string","enum":["result","question","status","ack","correction"]},"repo":string(),"assignee":string(),"next_step":string(),"blocker":string()})),"decisions":strings(),
        "dispositions":{"type":"array","items":{"anyOf":[declined,deferred]}},"asks":{"type":"array","items":object(json!({"summary":string(),"due":{"type":"number"}}))},"reopen_blocked":{"type":"boolean"}}),
    )
}
