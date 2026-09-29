//! Optional campaign planning. Models can propose wording; these rules determine
//! readiness. GitHub reviews always retain the authenticated reviewer's identity.
pub mod executor;
pub mod registry;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Validation {
    pub requirement: String,
    pub commit: String,
    pub tree: String,
    pub passed: bool,
    pub job_id: String,
    pub verified: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Review {
    pub reviewer: String,
    pub commit: String,
    pub approved: bool,
    pub verified_by_api: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub commit: String,
    pub completed: bool,
    pub conclusion: String,
    pub verified_by_api: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: String,
    pub author: String,
    pub head: String,
    pub tree: String,
    pub owner_paused: bool,
    pub linked_thread_paused: bool,
    pub stopped: bool,
    pub merge_predecessors_satisfied: bool,
    pub requirements: Vec<String>,
    pub required_checks: Vec<String>,
    pub allowed_reviewers: Vec<String>,
    pub validations: Vec<Validation>,
    pub checks: Vec<Check>,
    pub reviews: Vec<Review>,
    pub range_diff_changed: bool,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", content = "reason", rename_all = "snake_case")]
pub enum Decision {
    Blocked(String),
    Validate(Vec<String>),
    WaitForCi(Vec<String>),
    RequestIndependentReview,
    ReadyForHumanMerge,
}

pub fn plan(item: &Item) -> Decision {
    if item.stopped {
        return Decision::Blocked("campaign stopped".into());
    }
    if item.owner_paused || item.linked_thread_paused {
        return Decision::Blocked("owner paused a linked thread".into());
    }
    if item.requirements.is_empty()
        || item.required_checks.is_empty()
        || item.allowed_reviewers.is_empty()
    {
        return Decision::Blocked(
            "explicit validation, CI and independent reviewer requirements are required".into(),
        );
    }
    if item.head.is_empty() || item.tree.is_empty() {
        return Decision::Blocked("head commit and tree are unverified".into());
    }
    let missing: Vec<_> = item
        .requirements
        .iter()
        .filter(|name| {
            !item.validations.iter().any(|e| {
                &e.requirement == *name
                    && e.commit == item.head
                    && e.tree == item.tree
                    && e.passed
                    && e.verified
                    && !e.job_id.is_empty()
            })
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Decision::Validate(missing);
    }
    let missing: Vec<_> = item
        .required_checks
        .iter()
        .filter(|name| {
            !item.checks.iter().any(|e| {
                &e.name == *name
                    && e.commit == item.head
                    && e.completed
                    && e.conclusion == "success"
                    && e.verified_by_api
            })
        })
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Decision::WaitForCi(missing);
    }
    let approved = item.reviews.iter().any(|e| {
        e.commit == item.head
            && e.approved
            && e.verified_by_api
            && e.reviewer != item.author
            && item.allowed_reviewers.contains(&e.reviewer)
    });
    if !approved {
        return Decision::RequestIndependentReview;
    }
    if !item.merge_predecessors_satisfied {
        return Decision::Blocked("waiting for earlier human merges".into());
    }
    // A changed range never carries old evidence forward: every predicate above
    // matches the exact new commit (and the tested tree), regardless of range-diff.
    Decision::ReadyForHumanMerge
}
