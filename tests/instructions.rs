use fridica::{
    config::{contract, loader, repos, LoadContext},
    core::worker::WorkerRecord,
    workers::{
        instructions::{OwnerInstructions, UNTRUSTED},
        jsonl::Instructions,
    },
};
use serde_json::{json, Value};
#[test]
fn frozen_contracts_repositories_and_worker_prompt_match() {
    let corpus: Value =
        serde_json::from_str(include_str!("corpus/approval-instructions.json")).unwrap();
    for case in corpus["contracts"].as_array().unwrap() {
        let result = contract::parse(case["text"].as_str().unwrap());
        if case["expected"].is_null() {
            assert!(result.is_err());
            continue;
        }
        let result = result.unwrap();
        let mut actual = serde_json::to_value(&result).unwrap();
        actual["worker"] = json!(result.worker());
        actual["parent"] = json!(result.parent());
        assert_eq!(actual, case["expected"]);
    }
    for case in corpus["repos"].as_array().unwrap() {
        let result = repos::parse(case["text"].as_str().unwrap());
        if case["expected"].is_null() {
            assert!(result.is_err(), "accepted {case}");
            continue;
        }
        assert_eq!(
            serde_json::to_value(result.unwrap()).unwrap(),
            case["expected"]
        );
    }
    assert_eq!(
        format!("{}\n{UNTRUSTED}", contract::load(None).unwrap().worker()),
        corpus["prompt"]["prefix"]
    );
    assert_eq!(
        serde_json::to_value(repos::load(None).unwrap()).unwrap(),
        corpus["prompt"]["data"]["repositories"]
    );
}
#[test]
fn owner_overrides_inherit_only_optional_sections_and_reload_without_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("owner.md");
    std::fs::write(
        &path,
        "## Participation\np\n## Replies\nr\n## Lab rules\nprivate rule",
    )
    .unwrap();
    let owner = contract::load(Some(&path)).unwrap();
    let defaults = contract::load(None).unwrap();
    assert_eq!(owner.delegation, defaults.delegation);
    assert_eq!(owner.workers, defaults.workers);
    assert_eq!(owner.debriefs, defaults.debriefs);
    assert_eq!(owner.extra, "## Lab rules\n\nprivate rule");
    let corpus: Value = serde_json::from_str(include_str!("corpus/placement.json")).unwrap();
    let mut config = loader::parse(
        corpus["source"].as_str().unwrap(),
        &dir.path().join("config.toml"),
        &LoadContext {
            home: "/tmp/test-home".into(),
            uid: 1,
            runtime_dir: None,
            protected: vec![],
        },
    )
    .unwrap();
    config.owner.contract = Some(path.clone());
    config.owner.profile = "雪 profile".into();
    let repos_path = dir.path().join("repos.toml");
    std::fs::write(
        &repos_path,
        "[[repos]]\nname='r'\nurl='https://example.com/o/r'\ncollaborators=['owner']",
    )
    .unwrap();
    config.parent.repos = Some(repos_path.clone());
    let mut worker: WorkerRecord = serde_json::from_value(
        json!({"id":"w1","session_id":"s1","machine":"gpu","workspace":"unique","backend":"codex"}),
    )
    .unwrap();
    let prompt = OwnerInstructions.build(&config, &worker).unwrap();
    assert!(prompt.contains("private rule"));
    assert!(!prompt.contains("Delegate work"));
    assert!(!prompt.contains("/work/") && !prompt.contains("/tmp/test-home"));
    let data: Value =
        serde_json::from_str(prompt.split_once("\n\nWorker data:\n").unwrap().1).unwrap();
    assert_eq!(data["owner_id"], "UOWNER");
    assert_eq!(data["profile"], "雪 profile");
    assert_eq!(data["repositories"][0]["owner"], "owner");
    assert_eq!(data["workspace"], "unique");
    assert_eq!(
        data["machine"],
        config.machines.get("gpu").unwrap().payload(0)
    );
    std::fs::write(
        &path,
        "## Participation\np\n## Replies\nr\n## Workers\nChanged instructions",
    )
    .unwrap();
    let changed = OwnerInstructions.build(&config, &worker).unwrap();
    assert!(changed.starts_with("Changed instructions\n"));
    assert!(!changed.contains("private rule"));
    std::fs::write(&repos_path, "repos=[]").unwrap();
    assert!(OwnerInstructions
        .build(&config, &worker)
        .unwrap()
        .contains("\"repositories\":[]"));
    std::fs::write(&path, "missing required sections").unwrap();
    assert!(OwnerInstructions.build(&config, &worker).is_err());
    worker.workspace = "missing".into();
    assert!(OwnerInstructions.build(&config, &worker).is_err());
    worker.machine = "missing".into();
    assert!(OwnerInstructions.build(&config, &worker).is_err());
}
#[test]
fn reads_are_utf8_and_byte_bounded_and_fail_closed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rules");
    assert!(contract::load(Some(&path)).is_err());
    assert!(repos::load(Some(&path)).is_err());
    for bytes in [
        vec![b'x'; contract::LIMIT + 1],
        vec![0xff],
        "雪".repeat(contract::LIMIT / 2).into_bytes(),
    ] {
        std::fs::write(&path, &bytes).unwrap();
        assert!(contract::load(Some(&path)).is_err());
        assert!(repos::load(Some(&path)).is_err());
    }
    let secret = "never-print-this-note";
    std::fs::write(&path, format!("notes=['{secret}'")).unwrap();
    assert!(!format!("{:#}", repos::load(Some(&path)).err().unwrap()).contains(secret));
}
