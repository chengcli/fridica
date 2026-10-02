use fridica::{
    core::{delivery::AdapterFuture, time::ReplayClock},
    github::{
        client::{Api, Failure, Operation, Request},
        links::{self, Links, Reader},
        view,
    },
    store::Store,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
fn corpus() -> Value {
    serde_json::from_str(include_str!("corpus/github.json")).unwrap()
}
#[derive(Default)]
struct Script {
    routes: Mutex<Value>,
    calls: Mutex<Vec<String>>,
}
impl Script {
    fn new(routes: Value) -> Self {
        Self {
            routes: Mutex::new(routes),
            calls: Mutex::new(vec![]),
        }
    }
}
impl Api for Script {
    fn get(&self, request: Request) -> AdapterFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            let path = request.endpoint()?;
            self.calls.lock().unwrap().push(path.clone());
            let mut routes = self.routes.lock().unwrap();
            let key = if routes.get(&path).is_some() {
                &path
            } else {
                path.split('?').next().unwrap()
            };
            let Some(value) = routes.get_mut(key) else {
                return Err(Failure::NotFound);
            };
            let value =
                if let Some(seq) = value.get_mut("__responses").and_then(Value::as_array_mut) {
                    if seq.len() > 1 {
                        seq.remove(0)
                    } else {
                        seq[0].clone()
                    }
                } else {
                    value.clone()
                };
            if value.get("__error").is_some() {
                return Err(match value["__error"].as_str().unwrap() {
                    "recording" => Failure::Recording,
                    "rate" => Failure::RateLimited { after: 60. },
                    _ => Failure::Http { status: 502 },
                });
            }
            Ok(value)
        })
    }
}
#[test]
fn frozen_github_parsers_status_and_claims_match() {
    let corpus = corpus();
    for case in corpus["parse"].as_array().unwrap() {
        let texts: Vec<String> = serde_json::from_value(case["texts"].clone()).unwrap();
        assert_eq!(
            json!(links::links(&texts)),
            *case.get("rust_expected").unwrap_or(&case["expected"]),
            "{}",
            case["name"]
        );
    }
    for case in corpus["status"].as_array().unwrap() {
        assert_eq!(
            json!(view::status_lines(&case["body"])),
            case["expected"],
            "{}",
            case["name"]
        );
    }
    for case in corpus["claims"].as_array().unwrap() {
        assert_eq!(
            json!(view::claim_sha(case["text"].as_str().unwrap()).unwrap_or_default()),
            case["expected"],
            "{}",
            case["text"]
        );
    }
}
#[tokio::test]
async fn frozen_github_state_projections_and_request_multisets_match() {
    for case in corpus()["states"].as_array().unwrap() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let script = Arc::new(Script::new(case["routes"].clone()));
        let clock = Arc::new(ReplayClock::new(1000.));
        let links = Links::new(script.clone(), store, clock, 180.).unwrap();
        let output = links
            .linked(serde_json::from_value(case["texts"].clone()).unwrap())
            .await
            .unwrap();
        // Fresh reads report age 0; the frozen projections predate the field.
        let mut output = json!(output);
        for item in output.as_array_mut().into_iter().flatten() {
            if let Some(item) = item.as_object_mut() {
                assert_eq!(
                    item.remove("age_seconds"),
                    Some(json!(0.)),
                    "{}",
                    case["name"]
                );
            }
        }
        assert_eq!(
            output,
            *case.get("rust_expected").unwrap_or(&case["expected"]),
            "{}",
            case["name"]
        );
        let mut calls = script.calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(json!(calls), case["calls"], "{}", case["name"]);
    }
}
#[tokio::test]
async fn cache_expiry_rate_limits_and_recording_failures_are_visible() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    let clock = Arc::new(ReplayClock::new(1000.));
    let script = Arc::new(Script::new(
        json!({"/repos/o/r/issues/1":{"number":1,"title":"cached","state":"open"},"/repos/o/r/issues/2":{"__error":"rate"},"/repos/o/r/issues/3":{"__error":"recording"}}),
    ));
    let reader = Links::new(script.clone(), store.clone(), clock.clone(), 10.).unwrap();
    let url = |n| vec![format!("https://github.com/o/r/issues/{n}")];
    let first = reader.linked(url(1)).await.unwrap();
    assert_eq!(
        reader
            .linked(vec!["https://github.com/O/R/pull/1".into()])
            .await
            .unwrap(),
        first
    );
    assert_eq!(script.calls.lock().unwrap().len(), 1);
    clock.set(1011.);
    reader.linked(url(1)).await.unwrap();
    assert_eq!(script.calls.lock().unwrap().len(), 2);
    assert!(reader.linked(url(2)).await.unwrap()[0]["error"]
        .as_str()
        .unwrap()
        .contains("rate limit"));
    reader.linked(url(3)).await.unwrap();
    assert_eq!(script.calls.lock().unwrap().len(), 3);
    clock.set(1080.);
    assert_eq!(reader.linked(url(3)).await, Err(Failure::Recording));
    reader.linked(url(1)).await.unwrap();
    store.call(|c|{c.execute_batch("CREATE TRIGGER fail_cache BEFORE INSERT ON replay_events WHEN NEW.kind='github_cache_hit' BEGIN SELECT RAISE(ABORT,'private'); END;")?;Ok(())}).await.unwrap();
    assert_eq!(reader.linked(url(1)).await, Err(Failure::Recording));
}
#[test]
fn fixed_routes_reject_injection_and_bound_pagination() {
    for request in [
        Request {
            repo: "o/../secrets".into(),
            operation: Operation::Issue { number: 1 },
        },
        Request {
            repo: "o/r".into(),
            operation: Operation::Tree {
                head: "{owner}".into(),
            },
        },
        Request {
            repo: "o/r".into(),
            operation: Operation::Branch {
                name: "study/./secret".into(),
            },
        },
        Request {
            repo: "o/r".into(),
            operation: Operation::Compare {
                head: "a".repeat(40),
                base: "../../user".into(),
            },
        },
        Request {
            repo: "o/r".into(),
            operation: Operation::Reviews { number: 1, page: 4 },
        },
    ] {
        assert_eq!(request.endpoint(), Err(Failure::Invalid));
    }
    let request = Request {
        repo: "o/r".into(),
        operation: Operation::Compare {
            head: "a".repeat(40),
            base: "feature/{owner}?x=#evil".into(),
        },
    };
    assert_eq!(
        request.endpoint().unwrap(),
        format!(
            "/repos/o/r/compare/feature/%7Bowner%7D%3Fx%3D%23evil...{}",
            "a".repeat(40)
        )
    );
    assert_eq!(
        Request {
            repo: "o/r".into(),
            operation: Operation::Branch {
                name: "study/7".into(),
            },
        }
        .endpoint()
        .unwrap(),
        "/repos/o/r/git/ref/heads/study/7"
    );
}

#[tokio::test]
async fn one_bad_link_is_isolated_and_auxiliary_recording_failures_are_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    let clock = Arc::new(ReplayClock::new(1000.));
    let mut routes = corpus()["states"][0]["routes"].clone();
    routes["/repos/o/bad/pulls/1"] = json!(["bad"]);
    let script = Arc::new(Script::new(routes));
    let reader = Links::new(script.clone(), store, clock, 0.).unwrap();
    let values = reader
        .linked(vec![
            "https://github.com/o/bad/pull/1 https://github.com/o/r/pull/218".into(),
        ])
        .await
        .unwrap();
    assert_eq!(values[0]["error"], "unexpected response from GitHub");
    assert_eq!(values[1]["ci"], "success");
    let head = format!("924d2d8{}", "a".repeat(33));
    script.routes.lock().unwrap()[format!("/repos/o/r/git/commits/{head}")] =
        json!({"__error":"recording"});
    assert_eq!(
        reader
            .linked(vec!["https://github.com/o/r/pull/218".into()])
            .await,
        Err(Failure::Recording)
    );
}
#[tokio::test]
async fn github_cache_is_bounded_and_oversized_pages_never_certify_success() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    let clock = Arc::new(ReplayClock::new(1000.));
    let mut routes = json!({});
    for n in 1..=258 {
        routes[format!("/repos/o/r/issues/{n}")] =
            json!({"number":n,"title":"small","state":"open"});
    }
    let script = Arc::new(Script::new(routes));
    let reader = Links::new(script.clone(), store, clock.clone(), 180.).unwrap();
    for n in 1..=258 {
        reader
            .linked(vec![format!("https://github.com/o/r/issues/{n}")])
            .await
            .unwrap();
    }
    reader
        .linked(vec![
            "https://github.com/o/r/issues/1".into(),
            "https://github.com/o/r/issues/258".into(),
        ])
        .await
        .unwrap();
    assert_eq!(script.calls.lock().unwrap().len(), 259);
    let mut routes = corpus()["states"][0]["routes"].clone();
    let head = format!("924d2d8{}", "a".repeat(33));
    routes[format!("/repos/o/r/commits/{head}/check-runs")] = json!({"total_count":1,"check_runs":vec![json!({"status":"completed","conclusion":"success"});101]});
    *script.routes.lock().unwrap() = routes;
    clock.set(1181.);
    let values = reader
        .linked(vec!["https://github.com/o/r/pull/218".into()])
        .await
        .unwrap();
    assert_eq!(values[0]["ci"], "incomplete");
    assert_eq!(values[0]["checks"]["success"], 100);
}

struct AuditFaultWithHungSibling {
    auxiliary: bool,
}
impl Api for AuditFaultWithHungSibling {
    fn get(&self, request: Request) -> AdapterFuture<'_, Result<Value, Failure>> {
        Box::pin(async move {
            if self.auxiliary {
                match request.operation {
                    Operation::Pull { .. } => {
                        Ok(corpus()["states"][0]["routes"]["/repos/o/r/pulls/218"].clone())
                    }
                    Operation::Tree { .. } => Err(Failure::Recording),
                    _ => std::future::pending().await,
                }
            } else if request.repo == "o/fault" {
                Err(Failure::Recording)
            } else {
                std::future::pending().await
            }
        })
    }
}
#[tokio::test]
async fn hung_sibling_cannot_hide_a_recording_fault_behind_optional_timeout() {
    for auxiliary in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        let clock = Arc::new(ReplayClock::new(1000.));
        let reader = Links::new(
            Arc::new(AuditFaultWithHungSibling { auxiliary }),
            store,
            clock,
            180.,
        )
        .unwrap();
        let text = if auxiliary {
            "https://github.com/o/r/pull/218"
        } else {
            "https://github.com/o/hang/pull/1 https://github.com/o/fault/pull/1"
        };
        assert_eq!(
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                reader.linked(vec![text.into()])
            )
            .await
            .unwrap(),
            Err(Failure::Recording)
        );
    }
}
#[tokio::test]
async fn cached_github_state_reports_its_age() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("db")).await.unwrap();
    let clock = Arc::new(ReplayClock::new(1000.));
    let script = Arc::new(Script::new(
        json!({"/repos/o/r/issues/1":{"number":1,"title":"cached","state":"open"}}),
    ));
    let reader = Links::new(script.clone(), store, clock.clone(), 60.).unwrap();
    let url = vec!["https://github.com/o/r/issues/1".to_string()];
    let first = json!(reader.linked(url.clone()).await.unwrap());
    assert_eq!(first[0]["age_seconds"], 0.);
    clock.set(1045.);
    let cached = json!(reader.linked(url.clone()).await.unwrap());
    assert_eq!(cached[0]["age_seconds"], 45.);
    assert_eq!(script.calls.lock().unwrap().len(), 1);
    // Past the cache lifetime the state is read again.
    clock.set(1061.);
    let fresh = json!(reader.linked(url).await.unwrap());
    assert_eq!(fresh[0]["age_seconds"], 0.);
    assert_eq!(script.calls.lock().unwrap().len(), 2);
}
