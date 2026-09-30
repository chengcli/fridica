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
            &BTreeMap::new(),
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

#[test]
fn probed_saturation_steers_only_tag_based_choices() {
    use fridica::machines::probe::Assessment;
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
    let load = |entries: &[(&str, f64, bool)]| -> BTreeMap<String, Assessment> {
        entries
            .iter()
            .map(|(name, score, saturated)| {
                (
                    name.to_string(),
                    Assessment {
                        score: *score,
                        saturated: *saturated,
                        ..Default::default()
                    },
                )
            })
            .collect()
    };
    let pick = |selector: Selector, sticky: &str, load: &BTreeMap<String, Assessment>| {
        resolve(
            &config.machines,
            &selector,
            sticky,
            "",
            &BTreeMap::new(),
            load,
        )
        .unwrap()
        .machine
        .name
        .clone()
    };
    let cuda = || Selector {
        tags: vec!["cuda".into()],
        workspace: "shared".into(),
        ..Default::default()
    };
    // No readings: the job-count rule, ties in configuration order.
    assert_eq!(pick(cuda(), "", &load(&[])), "gpu");
    // A saturated match is avoided, and measured load ranks the rest.
    assert_eq!(pick(cuda(), "", &load(&[("gpu", 1.2, true)])), "gpu2");
    assert_eq!(
        pick(
            cuda(),
            "",
            &load(&[("gpu", 0.8, false), ("gpu2", 0.1, false)])
        ),
        "gpu2"
    );
    // The sticky machine is kept unless it is saturated.
    assert_eq!(
        pick(
            cuda(),
            "gpu",
            &load(&[("gpu", 0.8, false), ("gpu2", 0.1, false)])
        ),
        "gpu"
    );
    assert_eq!(pick(cuda(), "gpu", &load(&[("gpu", 1.0, true)])), "gpu2");
    // Everything saturated: still place, on the least loaded match.
    assert_eq!(
        pick(
            cuda(),
            "",
            &load(&[("gpu", 1.5, true), ("gpu2", 1.1, true)])
        ),
        "gpu2"
    );
    // An explicitly named machine and the untagged default are never redirected.
    let named = Selector {
        machine: "gpu".into(),
        workspace: "shared".into(),
        ..Default::default()
    };
    assert_eq!(pick(named, "", &load(&[("gpu", 2., true)])), "gpu");
    assert_eq!(
        pick(Selector::default(), "", &load(&[("cpu", 2., true)])),
        "cpu"
    );
}
