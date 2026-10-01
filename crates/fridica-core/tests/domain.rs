//! Core stands alone: delegation, placement and results over plain values.
use fridica_core::{
    config::{
        registry::{Machine, Policy, Registry, Resources, Workspace},
        Limits,
    },
    delegation::{prepare, Scope},
    ids::ThreadId,
    parent::{Decision, ParentRequest},
    result,
    time::SequenceIds,
};
use serde_json::json;

fn registry() -> Registry {
    Registry {
        machines: vec![Machine {
            name: "local".into(),
            transport: "local".into(),
            workspaces: vec![Workspace {
                name: "project".into(),
                path: "/work".into(),
                policy: Policy::default(),
                subfolders: false,
            }],
            backends: vec!["codex".into()],
            default_backend: "codex".into(),
            policy: Policy::default(),
            host: String::new(),
            tags: vec!["cpu".into()],
            resources: Resources::default(),
            max_workers: 2,
            max_jobs: 2,
            slurm: None,
            description: String::new(),
        }],
        default: "local".into(),
    }
}
fn request() -> ParentRequest {
    serde_json::from_value(
        json!({"inbox_id":7,"call":"decide","session":{"id":"T:C:1.1","channel":"C",
        "work":{"workers":[],"busy":{}},"context":{}},"trigger":{},"history":[],"obligations":[],
        "previous":null,"errors":[]}),
    )
    .unwrap()
}

#[test]
fn delegation_places_new_workers_only_where_allowed() {
    let decision: Decision =
        serde_json::from_value(json!({"delegations":[{"brief":"Run the checks","tags":["cpu"]}]}))
            .unwrap();
    let (limits, machines) = (Limits::default(), registry());
    let scope = |allowed| {
        Some(Scope {
            allowed,
            limits: &limits,
            machines: &machines,
        })
    };
    let ids = SequenceIds::default();
    let work = prepare(&decision, &request(), scope(true), Some(&ids)).unwrap();
    assert_eq!((work.workers.len(), work.jobs.len()), (1, 1));
    assert_eq!(
        (
            work.workers[0].machine.as_str(),
            work.workers[0].workspace.as_str()
        ),
        ("local", "project")
    );
    assert_eq!(work.context["machine"], "local");
    // Validation alone allocates nothing.
    assert!(prepare(&decision, &request(), scope(true), None)
        .unwrap()
        .jobs
        .is_empty());
    let refused = prepare(&decision, &request(), scope(false), None)
        .err()
        .unwrap();
    assert!(refused.to_string().contains("disabled"), "{refused}");
    assert!(prepare(&decision, &request(), None, None).is_err());
    let unknown: Decision =
        serde_json::from_value(json!({"delegations":[{"brief":"x","machine":"gpu9"}]})).unwrap();
    assert!(prepare(&unknown, &request(), scope(true), None).is_err());
}

#[test]
fn thread_ids_and_worker_results_round_trip() {
    let id: ThreadId = "T1:C2:100.1".parse().unwrap();
    assert_eq!(
        (id.channel.0.as_str(), id.to_string().as_str()),
        ("C2", "T1:C2:100.1")
    );
    assert!("T1:C2".parse::<ThreadId>().is_err());
    let parsed = result::parse(
        "```json\n{\"status\":\"done\",\"summary\":\"ok\",\"report\":\"All checks passed.\"}\n```",
    )
    .unwrap();
    assert_eq!(parsed.status, "done");
    assert_eq!(result::fallback("free text").status, "partial");
}
