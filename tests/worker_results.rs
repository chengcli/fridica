use fridica::{
    core::worker::ApprovalDecision,
    workers::{claude, codex, protocol::WorkerSpec, result},
};
use serde_json::{json, Value};
#[test]
fn frozen_worker_results_and_policy_envelopes_match() {
    let corpus: Value = serde_json::from_str(include_str!("corpus/workers.json")).unwrap();
    assert_eq!(result::schema(), corpus["schema"]);
    assert_eq!(result::FORMAT_NOTE, corpus["format_note"]);
    assert_eq!(result::SUMMARIZE_PROMPT, corpus["summarize_prompt"]);
    for (i, case) in corpus["coercions"].as_array().unwrap().iter().enumerate() {
        assert_eq!(
            json!(result::coerce(&case["input"])),
            case["expected"],
            "coercion {i}"
        );
    }
    for (i, case) in corpus["parses"].as_array().unwrap().iter().enumerate() {
        let text = case["text"].as_str().unwrap();
        assert_eq!(json!(result::parse(text)), case["parse"], "parse {i}");
        assert_eq!(result::prose(text), case["prose"], "prose {i}");
        assert_eq!(
            json!(result::fallback(text)),
            case["fallback"],
            "fallback {i}"
        );
    }
    for case in corpus["policies"].as_array().unwrap() {
        let spec: WorkerSpec = serde_json::from_value(case["spec"].clone()).unwrap();
        if spec.backend == "codex" {
            let mut expected: Vec<String> =
                serde_json::from_value(case["expected"]["command"].clone()).unwrap();
            expected.extend(
                serde_json::from_value::<Vec<String>>(
                    corpus["exceptions"]["codex_command_suffix"].clone(),
                )
                .unwrap(),
            );
            assert_eq!(codex::command(&spec, &[]), expected);
            assert_eq!(codex::sandbox_mode(&spec), case["expected"]["sandbox_mode"]);
            assert_eq!(
                codex::sandbox_policy(&spec),
                case["expected"]["sandbox_policy"]
            );
        } else {
            let mut expected: Vec<String> =
                serde_json::from_value(case["expected"]["command"].clone()).unwrap();
            let mut actual = claude::command(&spec, "previous-session", "previous-session");
            for command in [&mut actual, &mut expected] {
                let i = command.iter().position(|s| s == "--settings").unwrap() + 1;
                let settings: Value = serde_json::from_str(&command[i]).unwrap();
                assert_eq!(settings, case["expected"]["settings"]);
                command[i] = settings.to_string();
            }
            assert_eq!(actual, expected);
        }
    }
    for case in corpus["approvals"].as_array().unwrap() {
        let method = case["method"].as_str().unwrap();
        let kind = case["kind"].as_str().unwrap();
        let decision: ApprovalDecision = serde_json::from_value(case["decision"].clone()).unwrap();
        assert_eq!(
            codex::answer(method, kind, &case["params"], decision),
            case["answer"]
        );
        assert_eq!(
            json!(codex::describe(kind, &case["params"], "123")),
            case["description"]
        );
    }
}
