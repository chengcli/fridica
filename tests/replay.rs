use fridica::core::policy::{legacy_gate, GateInput, Verdict};
use serde::Deserialize;

#[derive(Deserialize)]
struct Fixture {
    id: String,
    input: GateInput,
    expected: Verdict,
}

#[test]
fn frozen_python_gate_matches_rust_without_exceptions() {
    let corpus = include_str!("corpus/gates.jsonl");
    let mut count = 0;
    for line in corpus.lines() {
        let case: Fixture = serde_json::from_str(line).unwrap();
        assert_eq!(legacy_gate(&case.input), case.expected, "{}", case.id);
        count += 1;
    }
    assert!(count >= 1000, "corpus unexpectedly small");
}
