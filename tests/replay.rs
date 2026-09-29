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

#[test]
fn frozen_parent_reply_rendering_matches() {
    let corpus: serde_json::Value =
        serde_json::from_str(include_str!("corpus/reply_rendering.json")).unwrap();
    let text = |value: &serde_json::Value| {
        corpus["strings"][value.as_u64().unwrap() as usize]
            .as_str()
            .unwrap()
    };
    for case in corpus["cases"].as_array().unwrap() {
        let i = &case["input"];
        let people = i["people"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s.as_str().unwrap().to_owned())
            .collect();
        let actual = fridica::core::render::reply(
            text(&i["text"]),
            text(&i["details"]),
            i["status"] == "waiting",
            i["requester"].as_str().unwrap(),
            &people,
            i["limit"].as_u64().unwrap() as usize,
        );
        assert_eq!(
            actual,
            (
                text(&case["expected"][0]).into(),
                text(&case["expected"][1]).into()
            ),
            "status={} limit={}",
            i["status"],
            i["limit"]
        );
    }
}
