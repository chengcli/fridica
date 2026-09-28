use fridica::{
    core::delivery::AdapterFuture,
    slack::files::{self, Download, Downloader, Failure},
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};
struct Script {
    data: BTreeMap<String, Value>,
    calls: Mutex<Vec<Value>>,
}
impl Downloader for Script {
    fn download(&self, url: String, html: bool) -> AdapterFuture<'_, Result<Download, Failure>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(json!({"url":url,"html":html}));
            let item = &self.data[&url];
            if item.get("error").is_some() {
                return Err(Failure::MissingScope);
            }
            let mut data = item["text"]
                .as_str()
                .unwrap_or("")
                .as_bytes()
                .repeat(item["repeat"].as_u64().unwrap_or(1) as usize);
            data.extend(
                item["suffix"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|v| v.as_u64().unwrap() as u8),
            );
            let size = item["size"].as_u64().unwrap_or(data.len() as u64);
            data.truncate(files::FILE_LIMIT + 1);
            Ok(Download { data, size })
        })
    }
}
#[tokio::test]
async fn frozen_python_attachment_views_match_exactly() {
    let cases: Value = serde_json::from_str(include_str!("corpus/attachments.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let script = Script {
            data: serde_json::from_value(case["data"].clone()).unwrap(),
            calls: Mutex::new(vec![]),
        };
        let own = serde_json::from_value(case["own"].clone()).unwrap();
        let views = files::read(
            &script,
            case["messages"].as_array().unwrap(),
            &own,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(serde_json::to_vec(&views).unwrap())),
            case["sha256"].as_str().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(json!(*script.calls.lock().unwrap()), case["calls"]);
    }
}
struct Blocked {
    active: AtomicUsize,
    entered: tokio::sync::Semaphore,
}
struct Guard<'a>(&'a AtomicUsize);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
impl Downloader for Blocked {
    fn download(&self, _: String, _: bool) -> AdapterFuture<'_, Result<Download, Failure>> {
        Box::pin(async move {
            self.active.fetch_add(1, Ordering::SeqCst);
            let _guard = Guard(&self.active);
            self.entered.add_permits(1);
            std::future::pending().await
        })
    }
}
#[tokio::test]
async fn downloads_are_concurrent_bounded_and_cancelled_on_timeout() {
    let downloader = Blocked {
        active: AtomicUsize::new(0),
        entered: tokio::sync::Semaphore::new(0),
    };
    let messages = vec![
        json!({"event_id":"e","attachments":(0..6).map(|n|json!({"id":format!("F{n}"),"name":"a.txt","url":"https://files.slack.com/a"})).collect::<Vec<_>>()}),
    ];
    let own = BTreeSet::new();
    let read = files::read(&downloader, &messages, &own, Duration::from_millis(100));
    tokio::pin!(read);
    tokio::select! { result=&mut read=>panic!("unexpected completion: {result:?}"), _=downloader.entered.acquire_many(3)=>{} }
    assert_eq!(downloader.active.load(Ordering::SeqCst), 3);
    let views = read.await.unwrap();
    assert_eq!(downloader.active.load(Ordering::SeqCst), 0);
    assert!(views["e"][..3]
        .iter()
        .all(|v| v["note"] == "not read: download timed out"));
    assert!(views["e"][3..]
        .iter()
        .all(|v| v["note"] == "not read: only 3 files are read per reply"));
}
