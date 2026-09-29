use fridica::{
    config::{loader, registry::slot_gpus, LoadContext},
    machines::{resolve, MatchError, Selector},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Deserialize)]
struct Case {
    selector: Selector,
    sticky_machine: String,
    sticky_workspace: String,
    busy: BTreeMap<String, usize>,
    expected: Option<Value>,
    error: Option<MatchError>,
}
#[derive(Deserialize)]
struct Gpus {
    gpus: Option<Vec<usize>>,
    slot: usize,
    slots: usize,
    expected: Option<Vec<usize>>,
}
#[derive(Deserialize)]
struct Corpus {
    source: String,
    payload: Value,
    cases: Vec<Case>,
    gpus: Vec<Gpus>,
}
#[test]
fn frozen_placement_diagnostics_payloads_and_gpu_views_match_exactly() {
    let corpus: Corpus = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
    let context = LoadContext {
        home: PathBuf::from("/tmp/fridica-test-home"),
        runtime_dir: None,
        uid: 123,
        protected: vec![],
    };
    let config = loader::parse(
        &corpus.source,
        &PathBuf::from("/tmp/fridica-test-config.toml"),
        &context,
    )
    .unwrap();
    assert_eq!(
        json!(config
            .machines
            .payload(&BTreeMap::from([("gpu".into(), 2)]))),
        corpus.payload
    );
    for (n, c) in corpus.cases.iter().enumerate() {
        match resolve(
            &config.machines,
            &c.selector,
            &c.sticky_machine,
            &c.sticky_workspace,
            &c.busy,
        ) {
            Ok(p) => assert_eq!(
                Some(
                    json!({"machine":p.machine.name,"workspace":p.workspace.name,"backend":p.backend})
                ),
                c.expected,
                "case {n}"
            ),
            Err(e) => assert_eq!(Some(e), c.error, "case {n}"),
        }
    }
    for c in corpus.gpus {
        assert_eq!(
            slot_gpus(&c.gpus, c.slot, c.slots),
            c.expected,
            "{:?}, {}, {}",
            c.gpus,
            c.slot,
            c.slots
        );
    }
    let gpu = config.machines.get("gpu").unwrap();
    assert_eq!(
        gpu.for_slot(2).resources.environment()["CUDA_VISIBLE_DEVICES"],
        "2,3"
    );
    assert_eq!(
        gpu.for_slot(2).resources.environment()["OMP_NUM_THREADS"],
        "32"
    );
    assert_eq!(
        gpu.workspace("unique").unwrap().for_slot(2).path,
        PathBuf::from("~/work/unique/worker2")
    );
    assert_eq!(gpu.resources.gpus, Some(vec![0, 1, 2, 3]));
}
