use fridica::{
    core::delivery::AdapterFuture,
    slack::links::{self, Entry, Failure, Link, Reader},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{sync::Mutex, time::Duration};
fn corpus() -> Value {
    serde_json::from_str(include_str!("corpus/links.json")).unwrap()
}
fn digest(value: &impl serde::Serialize) -> String {
    format!("{:x}", Sha256::digest(serde_json::to_vec(value).unwrap()))
}
#[test]
fn frozen_python_permalinks_and_fetch_selection() {
    let corpus = corpus();
    for case in corpus["parse"].as_array().unwrap() {
        let expected = case.get("rust_expected").unwrap_or(&case["expected"]);
        assert_eq!(
            json!(links::permalinks(case["text"].as_str().unwrap())),
            *expected,
            "{}",
            case["name"]
        );
    }
    for case in corpus["fetch"].as_array().unwrap() {
        // Serialize as Value for canonical sorted keys, as in the Python digest.
        let result = json!(links::select(
            case["messages"].as_array().unwrap(),
            case["ts"].as_str().unwrap()
        ));
        assert_eq!(
            digest(&result),
            case.get("rust_sha256")
                .unwrap_or(&case["sha256"])
                .as_str()
                .unwrap(),
            "{}",
            case["name"]
        );
    }
}
struct Script {
    data: Value,
    calls: Mutex<Vec<Value>>,
}
impl Reader for Script {
    fn fetch(&self, link: Link) -> AdapterFuture<'_, Result<Vec<Entry>, Failure>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(json!({"channel":link.channel,"ts":link.ts,"root":link.root}));
            let result = &self.data[&link.ts];
            if result == "error" {
                return Err(Failure::Unavailable);
            }
            Ok(result
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| Entry {
                    sender: v["sender"].as_str().unwrap().into(),
                    text: v["text"].as_str().unwrap().into(),
                    ts: link.ts.clone(),
                })
                .collect())
        })
    }
}
#[tokio::test]
async fn frozen_python_linked_context_matches() {
    for case in corpus()["context"].as_array().unwrap() {
        let reader = Script {
            data: case["data"].clone(),
            calls: Mutex::new(vec![]),
        };
        let result = links::read(
            &reader,
            &["CROOM".into()],
            "CROOM",
            "100.000001",
            case["messages"].as_array().unwrap(),
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            digest(&result),
            case["sha256"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(
            json!(*reader.calls.lock().unwrap()),
            case["calls"],
            "{}",
            case["name"]
        );
    }
}
struct Blocked {
    calls: Mutex<usize>,
    recording: bool,
}
impl Reader for Blocked {
    fn fetch(&self, _: Link) -> AdapterFuture<'_, Result<Vec<Entry>, Failure>> {
        Box::pin(async move {
            *self.calls.lock().unwrap() += 1;
            if self.recording {
                Err(Failure::Recording)
            } else {
                std::future::pending().await
            }
        })
    }
}
#[tokio::test]
async fn linked_reads_share_deadline_and_recording_faults_are_fatal() {
    let messages = vec![
        json!({"text":"https://t.slack.com/archives/CROOM/p200000001 https://t.slack.com/archives/CROOM/p201000001"}),
    ];
    for recording in [false, true] {
        let reader = Blocked {
            calls: Mutex::new(0),
            recording,
        };
        let result = links::read(
            &reader,
            &["CROOM".into()],
            "CROOM",
            "100.000001",
            &messages,
            Duration::from_millis(10),
        )
        .await;
        assert_eq!(*reader.calls.lock().unwrap(), 1);
        if recording {
            assert_eq!(result, Err(Failure::Recording));
        } else {
            let result = result.unwrap();
            assert_eq!(result.len(), 2);
            assert!(result.iter().all(|v| v["error"] == "could not be fetched"));
        }
    }
}
