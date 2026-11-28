//! Assignment IDs are vertices; each dependency names a prerequisite in this project.
//! Visit prerequisites first so readiness includes indirect blockers.
use crate::core::parent::ReplyStatus;
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

// Share the thread reply vocabulary. Ready and working describe dispatch.
pub(super) fn statuses() -> [&'static str; 5] {
    [
        "ready",
        "working",
        ReplyStatus::Waiting.as_str(),
        ReplyStatus::Blocked.as_str(),
        ReplyStatus::Complete.as_str(),
    ]
}

pub(super) fn reported(status: &str, legacy_finished: Option<bool>) -> &'static str {
    // Older records could describe themselves as complete while explicitly open.
    if legacy_finished == Some(false) && status.trim().eq_ignore_ascii_case("complete") {
        return ReplyStatus::Blocked.as_str();
    }
    statuses()
        .into_iter()
        .find(|s| status.trim().eq_ignore_ascii_case(s))
        .unwrap_or(if legacy_finished == Some(true) {
            ReplyStatus::Complete.as_str()
        } else {
            "ready"
        })
}

#[derive(Serialize)]
pub(super) struct Graph<'a> {
    pub status: BTreeMap<&'a str, &'static str>,
    pub ready: BTreeSet<&'a str>,
    pub blocked_by: BTreeMap<&'a str, Vec<&'a str>>,
    pub verified: BTreeSet<&'a str>,
}

impl<'a> Graph<'a> {
    pub fn build(assignments: &'a Map<String, Value>) -> Result<Self> {
        let mut dependents = BTreeMap::<&str, Vec<&str>>::new();
        let mut remaining = BTreeMap::new();
        let mut queue = VecDeque::new();
        for (id, assignment) in assignments {
            let dependencies = assignment["dependencies"]
                .as_array()
                .context("missing dependencies")?;
            let mut seen = BTreeSet::new();
            for dependency in dependencies {
                let dep = dependency
                    .as_str()
                    .context("dependency must be an assignment ID")?;
                ensure!(assignments.contains_key(dep), "assignment {id} has unknown prerequisite {dep}; define its assignment or correct the reference");
                ensure!(dep != id, "assignment {id} depends on itself");
                ensure!(seen.insert(dep), "duplicate prerequisite {dep} on {id}");
                dependents.entry(dep).or_default().push(id);
            }
            remaining.insert(id.as_str(), dependencies.len());
            if dependencies.is_empty() {
                queue.push_back(id.as_str());
            }
        }

        let mut graph = Graph {
            status: BTreeMap::new(),
            ready: BTreeSet::new(),
            blocked_by: BTreeMap::new(),
            verified: BTreeSet::new(),
        };
        while let Some(id) = queue.pop_front() {
            let a = &assignments[id];
            let blockers: Vec<_> = a["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(Value::as_str)
                .filter(|dep| !graph.verified.contains(dep))
                .collect();
            let reported = reported(
                a["status"].as_str().unwrap_or_default(),
                a["finished"].as_bool(),
            );
            let status = if !blockers.is_empty() {
                graph.blocked_by.insert(id, blockers);
                ReplyStatus::Blocked.as_str()
            } else if reported == ReplyStatus::Complete.as_str() {
                if a["evidence"].as_str().is_some_and(|s| !s.trim().is_empty()) {
                    graph.verified.insert(id);
                    reported
                } else {
                    ReplyStatus::Blocked.as_str()
                }
            } else {
                reported
            };
            graph.status.insert(id, status);
            if status == "ready" {
                graph.ready.insert(id);
            }
            for dependent in dependents.get(id).into_iter().flatten() {
                let count = remaining.get_mut(dependent).unwrap();
                *count -= 1;
                if *count == 0 {
                    queue.push_back(*dependent);
                }
            }
        }
        if let Some((id, _)) = remaining.iter().find(|(_, count)| **count != 0) {
            anyhow::bail!("dependency cycle prevents ordering assignment {id}; correct the prerequisite links");
        }
        Ok(graph)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_missing_duplicate_self_and_cyclic_prerequisites() {
        for (a, b, msg) in [
            (vec!["external label"], vec![], "unknown prerequisite"),
            (vec!["b", "b"], vec![], "duplicate prerequisite"),
            (vec!["a"], vec![], "depends on itself"),
            (vec!["b"], vec!["a"], "cycle"),
        ] {
            let nodes = json!({"a":{"dependencies":a},"b":{"dependencies":b}});
            let error = Graph::build(nodes.as_object().unwrap()).err().unwrap();
            assert!(error.to_string().contains(msg), "{error}");
        }
    }

    #[test]
    fn clearing_a_prerequisite_preserves_reported_status() {
        let mut nodes = json!({"root":{"dependencies":[],"status":"working"}});
        for status in ["ready", "working", "waiting", "blocked"] {
            nodes[status] = json!({"dependencies":["root"],"status":status});
        }
        let graph = Graph::build(nodes.as_object().unwrap()).unwrap();
        assert!(graph.ready.is_empty());
        assert_eq!(graph.status["waiting"], "blocked");
        nodes["root"]["status"] = json!("complete");
        nodes["root"]["evidence"] = json!("verified receipt");
        let graph = Graph::build(nodes.as_object().unwrap()).unwrap();
        assert_eq!(graph.ready, BTreeSet::from(["ready"]));
        for status in ["working", "waiting", "blocked"] {
            assert_eq!(graph.status[status], status);
        }
        assert!(graph.blocked_by.is_empty());
        assert_eq!(graph.verified, BTreeSet::from(["root"]));
    }

    #[test]
    fn checks_all_paths_before_unlocking_dependents() {
        let nodes = json!({"root":{"dependencies":[],"finished":true,"evidence":"receipt"},
            "left":{"dependencies":["root"],"finished":true,"evidence":"receipt"},
            "right":{"dependencies":["root"],"finished":true,"evidence":""},
            "join":{"dependencies":["left","right"],"finished":true,"evidence":"old receipt"},
            "tail":{"dependencies":["join"]}});
        let g = Graph::build(nodes.as_object().unwrap()).unwrap();
        assert_eq!(reported("complete", Some(false)), "blocked");
        assert!(g.ready.is_empty());
        assert_eq!(g.status["right"], "blocked");
        assert_eq!(g.verified, BTreeSet::from(["root", "left"]));
        assert_eq!(g.blocked_by["join"], ["right"]);
        assert_eq!(g.blocked_by["tail"], ["join"]);
    }
}
