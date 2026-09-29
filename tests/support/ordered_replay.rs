//! Complete synthetic adapter tapes plus consistent, all-column database snapshots.
//! Raw snapshots are private; committed fixtures contain only synthetic content.
use fridica::{
    core::time::{Clock, ReplayClock},
    store::Store,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub seq: usize,
    pub at: f64,
    pub kind: String,
    pub payload: Value,
}
pub struct Tape {
    clock: Arc<ReplayClock>,
    bindings: Mutex<Vec<(String, String)>>,
    state: Mutex<State>,
    changed: tokio::sync::Notify,
}
struct State {
    rows: Vec<Event>,
    cursor: Option<usize>,
    calls: usize,
}
impl Tape {
    pub fn record(clock: Arc<ReplayClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            bindings: Mutex::new(vec![]),
            state: Mutex::new(State {
                rows: vec![],
                cursor: None,
                calls: 0,
            }),
            changed: tokio::sync::Notify::new(),
        })
    }
    pub fn replay(rows: Vec<Event>, clock: Arc<ReplayClock>) -> anyhow::Result<Arc<Self>> {
        validate(&rows)?;
        Ok(Arc::new(Self {
            clock,
            bindings: Mutex::new(vec![]),
            state: Mutex::new(State {
                rows,
                cursor: Some(0),
                calls: 0,
            }),
            changed: tokio::sync::Notify::new(),
        }))
    }
    pub fn bind(&self, root: &str, fingerprint: &str) {
        *self.bindings.lock().unwrap() = vec![
            (root.into(), "__ROOT__".into()),
            (fingerprint.into(), "__CONFIG__".into()),
        ];
    }
    pub fn normalize(&self, value: Value) -> Value {
        let mut raw = value.to_string();
        for (from, to) in self.bindings.lock().unwrap().iter() {
            raw = raw.replace(from, to);
        }
        serde_json::from_str(&raw).unwrap()
    }
    pub fn begin(&self, operation: &str, arguments: Value) -> usize {
        let mut state = self.state.lock().unwrap();
        state.calls += 1;
        let id = state.calls;
        let payload = json!({"id":id,"operation":operation,"arguments":self.normalize(arguments)});
        if let Some(index) = state.cursor {
            let row = &state.rows[index];
            assert_eq!(row.kind, "call", "sequence {}", row.seq);
            assert_eq!(row.payload, payload, "sequence {}", row.seq);
            self.clock.set(row.at);
            state.cursor = Some(index + 1);
        } else {
            let seq = state.rows.len() + 1;
            state.rows.push(Event {
                seq,
                at: self.clock.now(),
                kind: "call".into(),
                payload,
            });
        }
        self.changed.notify_waiters();
        id
    }
    pub async fn finish(&self, id: usize, outcome: Value) -> Value {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let changed = self.changed.notified();
                // Register before inspecting the cursor, including multi-thread runtimes.
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let mut state = self.state.lock().unwrap();
                    if let Some(index) = state.cursor {
                        let row = state.rows.get(index).expect("replay exhausted");
                        if row.kind == "result" && row.payload["id"] == id {
                            let value = row.payload["value"].clone();
                            self.clock.set(row.at);
                            state.cursor = Some(index + 1);
                            self.changed.notify_waiters();
                            return value;
                        }
                    } else {
                        let seq = state.rows.len() + 1;
                        state.rows.push(Event {
                            seq,
                            at: self.clock.now(),
                            kind: "result".into(),
                            payload: json!({"id":id,"value":outcome}),
                        });
                        return outcome;
                    }
                }
                changed.await;
            }
        })
        .await
        .expect("replay schedule stalled")
    }
    pub fn rows(&self) -> Vec<Event> {
        let state = self.state.lock().unwrap();
        if let Some(index) = state.cursor {
            assert_eq!(index, state.rows.len(), "unconsumed tape events");
        }
        validate(&state.rows).unwrap();
        state.rows.clone()
    }
}
pub fn validate(rows: &[Event]) -> anyhow::Result<()> {
    let mut pending = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for (index, row) in rows.iter().enumerate() {
        anyhow::ensure!(
            row.seq == index + 1 && row.at.is_finite(),
            "invalid ordering or clock"
        );
        let id = row.payload["id"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("missing call id"))?;
        match row.kind.as_str() {
            "call" => {
                anyhow::ensure!(
                    seen.insert(id)
                        && row.payload["operation"].is_string()
                        && row.payload.get("arguments").is_some(),
                    "invalid call"
                );
                pending.insert(id);
            }
            "result" => {
                anyhow::ensure!(
                    pending.remove(&id) && row.payload.get("value").is_some(),
                    "orphan or duplicate result"
                );
            }
            _ => anyhow::bail!("unknown tape event"),
        }
    }
    anyhow::ensure!(
        pending.is_empty(),
        "incomplete tape: unknown external outcomes"
    );
    Ok(())
}

pub async fn snapshot(store: &Store) -> Value {
    store
        .call(|c| {
            let tx = c.transaction()?;
            let tables: Vec<String> = tx
                .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            let mut result = serde_json::Map::new();
            for table in tables {
                let mut query = tx.prepare(&format!(
                    "SELECT * FROM \"{}\" ORDER BY rowid",
                    table.replace('"', "\"\"")
                ))?;
                let names: Vec<String> =
                    query.column_names().iter().map(|s| s.to_string()).collect();
                let mut rows = query.query([])?;
                let mut values = vec![];
                while let Some(row) = rows.next()? {
                    let mut value = serde_json::Map::new();
                    for (index, name) in names.iter().enumerate() {
                        use rusqlite::types::ValueRef;
                        let field = match row.get_ref(index)? {
                            ValueRef::Null => Value::Null,
                            ValueRef::Integer(n) => json!(n),
                            ValueRef::Real(n) => json!(n),
                            ValueRef::Text(s) => {
                                let s = std::str::from_utf8(s)?;
                                if name.ends_with("_json") {
                                    serde_json::from_str(s).unwrap_or_else(|_| json!(s))
                                } else {
                                    json!(s)
                                }
                            }
                            ValueRef::Blob(s) => json!({"bytes":s}),
                        };
                        value.insert(name.clone(), field);
                    }
                    values.push(Value::Object(value));
                }
                result.insert(table, json!(values));
            }
            tx.commit()?;
            Ok(Value::Object(result))
        })
        .await
        .unwrap()
}
