//! The threads a thread is linked to through the channel ledger (#108): ones
//! it points at, ones that point at it, and ones that talk about the same pull
//! request or issue. Each is given to the parent as a bounded view of its own
//! state, so a check-up posted as a new thread is answered from the facts of
//! the threads where the work happens.
use crate::store::Sqlite;
use anyhow::Result;
use fridica_core::store::LinkedThreads;
use rusqlite::Connection;
use serde_json::{json, Value};

/// At most this many linked threads, most recently active first.
pub const THREADS: usize = 4;
/// Two threads share an item only when both mentioned it within this many
/// seconds of each other, so a number reused months later links nothing.
pub const WINDOW: f64 = 14. * 86400.;

fn cut(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        return text.to_owned();
    }
    let kept: String = text.chars().take(cap.saturating_sub(2)).collect();
    format!("{}[…]", kept.trim_end())
}

pub(super) fn views(c: &Connection, session: &str) -> Result<Vec<Value>> {
    let mut views = vec![];
    for thread in Sqlite(c).linked_threads(session, THREADS, WINDOW)? {
        let decisions: Vec<String> = serde_json::from_str::<Vec<String>>(&thread.decisions)
            .unwrap_or_default()
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|d| cut(d, 200))
            .collect();
        let notes = super::effects::notes(c, &thread.id)?.1;
        let asks: Vec<Value> = thread
            .asks
            .iter()
            .map(|a| json!({"kind":a.kind,"summary":cut(&a.summary,200),"due":a.due}))
            .collect();
        let jobs: Vec<Value> = thread
            .jobs
            .iter()
            .map(|j| {
                let mut job = json!({"id":j.id,"status":j.status,"role":j.role,
                    "machine":j.machine,"brief":cut(&j.brief,200),
                    "summary":cut(&j.summary,300),"error":j.error});
                if !j.progress.is_empty() {
                    job["progress"] = json!(cut(&j.progress, 300));
                }
                job
            })
            .collect();
        let messages: Vec<Value> = thread
            .messages
            .iter()
            .map(|m| {
                json!({"ts":m.ts,"sender":m.sender,
                    "text":cut(&m.text,300),"from_agent":m.from_agent})
            })
            .collect();
        let items: Vec<&str> = thread
            .shared_items
            .split(',')
            .filter(|s| !s.is_empty())
            .collect();
        views.push(json!({
            "thread":thread.root_ts,"status":thread.status,"control":thread.control,"root":cut(&thread.root,300),
            "linked_by":{"shared_items":items,"this_thread_refers_to_it":thread.referenced,"it_refers_to_this_thread":thread.references_this},
            "summary":cut(&thread.summary,600),"notes":cut(&notes.to_string(),600),"decisions":decisions,
            "open_asks":asks,"jobs":jobs,"latest_messages":messages,
        }));
    }
    Ok(views)
}
