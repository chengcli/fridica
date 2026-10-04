use fridica::{
    core::{
        delivery::{AdapterFuture, ClaimedPost, Delivery, DeliveryOutcome, Post},
        time::ReplayClock,
    },
    store::{outbox, Store},
    threads::dispatcher::Dispatcher,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

struct Tape {
    state: Mutex<TapeState>,
}
struct TapeState {
    outcomes: VecDeque<Value>,
    calls: Vec<Value>,
    sent: usize,
    uploaded: usize,
}
impl Delivery for Tape {
    fn send(&self, claim: ClaimedPost) -> AdapterFuture<'_, DeliveryOutcome> {
        Box::pin(async move {
            let p = claim.post;
            let mut state = self.state.lock().unwrap();
            let upload = p.kind == "upload";
            state.calls.push(if upload {json!({"kind":"upload","channel":p.channel,"thread_ts":p.thread_ts,"blob":p.blob.unwrap_or_default(),"filename":p.filename})}
                else {json!({"kind":"post","channel":p.channel,"thread_ts":p.thread_ts,"text":p.text,"meta":p.meta})});
            let value = state
                .outcomes
                .pop_front()
                .unwrap_or(json!({"outcome":"sent"}));
            if value["outcome"] == "sent" {
                if upload {
                    state.uploaded += 1;
                    DeliveryOutcome::Sent {
                        reference: format!("F{}", state.uploaded),
                    }
                } else {
                    state.sent += 1;
                    DeliveryOutcome::Sent {
                        reference: format!("200.{:06}", state.sent),
                    }
                }
            } else {
                serde_json::from_value(value).unwrap()
            }
        })
    }
}
#[derive(Deserialize)]
struct Corpus {
    fixtures: Vec<Fixture>,
}
#[derive(Deserialize)]
struct Fixture {
    name: String,
    posts: Vec<Post>,
    outcomes: Vec<Value>,
    snapshots: Vec<Snapshot>,
}
#[derive(Deserialize)]
struct Snapshot {
    at: f64,
    sent: usize,
    expected: Value,
}

#[tokio::test]
async fn delivery_replays_frozen_python_outcomes_history_and_order_exactly() {
    let corpus: Corpus = serde_json::from_str(include_str!("corpus/outbox.json")).unwrap();
    for fixture in corpus.fixtures {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("db")).await.unwrap();
        for post in fixture.posts {
            outbox::enqueue(&store, post, 1.).await.unwrap();
        }
        let clock = Arc::new(ReplayClock::new(1.));
        let tape = Arc::new(Tape {
            state: Mutex::new(TapeState {
                outcomes: fixture.outcomes.into(),
                calls: vec![],
                sent: 0,
                uploaded: 0,
            }),
        });
        let dispatcher = Dispatcher {
            store: Arc::new(store.clone()),
            delivery: tape.clone(),
            clock: clock.clone(),
            owner: "UOWNER".into(),
            observe_only: false,
            timeout: Duration::from_secs(1),
        };
        for snapshot in fixture.snapshots {
            clock.set(snapshot.at);
            assert_eq!(
                dispatcher.drain(100).await.unwrap(),
                snapshot.sent,
                "{} at {}",
                fixture.name,
                snapshot.at
            );
            let mut actual=store.call(|c| {
                let outbox:Vec<Value>=c.prepare("SELECT idem_key,state,attempts,retry_at,sent_ts FROM outbox ORDER BY id")?
                    .query_map([],|r|Ok(json!({"idem_key":r.get::<_,String>(0)?,"state":r.get::<_,String>(1)?,"attempts":r.get::<_,i64>(2)?,"retry_at":r.get::<_,f64>(3)?,"sent_ts":r.get::<_,String>(4)?})))?.collect::<rusqlite::Result<_>>()?;
                let history:Vec<Value>=c.prepare("SELECT json_object('workspace',workspace,'channel',channel,'ts',ts,'root_ts',root_ts,'thread_ts',thread_ts,'sender',sender,'text',text,'source',source,'meta',json(meta_json)) FROM messages ORDER BY id")?
                    .query_map([],|r|r.get::<_,String>(0))?.map(|r|Ok(serde_json::from_str(&r?)?)).collect::<anyhow::Result<_>>()?;
                Ok(json!({"outbox":outbox,"history":history}))
            }).await.unwrap();
            actual["calls"] = json!(tape.state.lock().unwrap().calls);
            assert_eq!(
                actual, snapshot.expected,
                "{} at {}",
                fixture.name, snapshot.at
            );
        }
        assert!(
            tape.state.lock().unwrap().outcomes.is_empty(),
            "unused outcomes: {}",
            fixture.name
        );
    }
}
