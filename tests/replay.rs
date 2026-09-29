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

#[test]
fn frozen_reply_hashes_and_repost_requests_match() {
    use fridica::core::policy::{reply_hash, repost_requested};
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("corpus/reply_policy.json")).unwrap();
    for row in corpus["repost"].as_array().unwrap() {
        let text = row["text"].as_str().unwrap();
        assert_eq!(
            repost_requested(text, "UOWNER"),
            row["expected"].as_bool().unwrap(),
            "{text:?}"
        );
    }
    for row in corpus["hashes"].as_array().unwrap() {
        let text = row["text"].as_str().unwrap();
        assert_eq!(
            reply_hash(text),
            row["expected"].as_str().unwrap(),
            "{text:?}"
        );
    }
}
