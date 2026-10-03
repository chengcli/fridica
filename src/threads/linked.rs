//! The threads a thread is linked to through the channel ledger (#108): ones
//! it points at, ones that point at it, and ones that talk about the same pull
//! request or issue. Each is given to the parent as a bounded view of its own
//! state, so a check-up posted as a new thread is answered from the facts of
//! the threads where the work happens.
use anyhow::Result;
use rusqlite::{params, Connection};
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
    // Why each thread is linked: shared items, and references either way.
    let rows: Vec<(String, String, bool, bool)> = c
        .prepare(
            "WITH mine AS (SELECT workspace,channel,item,repo,last_seen FROM item_links WHERE session_id=?1),
             shared AS (SELECT l.session_id AS id,group_concat(DISTINCT CASE WHEN l.repo='' THEN l.item ELSE l.repo||l.item END) AS items
                FROM item_links l JOIN mine m ON m.workspace=l.workspace AND m.channel=l.channel AND m.item=l.item
                WHERE l.session_id!=?1 AND (l.repo='' OR m.repo='' OR l.repo=m.repo) AND ABS(l.last_seen-m.last_seen)<=?3
                GROUP BY l.session_id),
             out AS (SELECT target AS id FROM thread_links WHERE session_id=?1),
             inc AS (SELECT session_id AS id FROM thread_links WHERE target=?1),
             ids AS (SELECT id FROM shared UNION SELECT id FROM out UNION SELECT id FROM inc)
             SELECT t.id,COALESCE((SELECT items FROM shared WHERE shared.id=t.id),''),
                EXISTS(SELECT 1 FROM out WHERE out.id=t.id),EXISTS(SELECT 1 FROM inc WHERE inc.id=t.id)
             FROM threads t JOIN ids ON ids.id=t.id
             WHERE t.control NOT IN ('archived','cleaned','closed') ORDER BY t.updated DESC,t.id LIMIT ?2",
        )?
        .query_map(params![session, THREADS as i64, WINDOW], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut views = vec![];
    for (id, items, referenced, references_this) in rows {
        let (root_ts, status, summary, decisions): (String, String, String, String) = c.query_row(
            "SELECT root_ts,status,summary,decisions_json FROM threads WHERE id=?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        let decisions: Vec<String> = serde_json::from_str::<Vec<String>>(&decisions)
            .unwrap_or_default()
            .iter()
            .rev()
            .take(3)
            .rev()
            .map(|d| cut(d, 200))
            .collect();
        let notes = super::effects::notes(c, &id)?.1;
        let asks: Vec<Value> = c
            .prepare(
                "SELECT kind,summary,due FROM obligations WHERE session_id=? AND state IN ('open','deferred')
                 ORDER BY created,id LIMIT 3",
            )?
            .query_map([&id], |r| {
                Ok(json!({"kind":r.get::<_,String>(0)?,"summary":cut(&r.get::<_,String>(1)?,200),"due":r.get::<_,f64>(2)?}))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let jobs: Vec<Value> = c
            .prepare(
                "SELECT j.id,j.status,w.role,w.machine,j.brief,COALESCE(json_extract(j.result_json,'$.summary'),''),j.error,
                    COALESCE((SELECT text FROM job_progress p WHERE p.job_id=j.id ORDER BY p.attempt DESC,p.seq DESC LIMIT 1),'')
                 FROM jobs j JOIN workers w ON w.id=j.worker_id WHERE j.session_id=? ORDER BY j.queued_at DESC,j.rowid DESC LIMIT 3",
            )?
            .query_map([&id], |r| {
                let mut job = json!({"id":r.get::<_,String>(0)?,"status":r.get::<_,String>(1)?,"role":r.get::<_,String>(2)?,
                    "machine":r.get::<_,String>(3)?,"brief":cut(&r.get::<_,String>(4)?,200),
                    "summary":cut(&r.get::<_,String>(5)?,300),"error":r.get::<_,String>(6)?});
                let progress: String = r.get(7)?;
                if !progress.is_empty() {
                    job["progress"] = json!(cut(&progress, 300));
                }
                Ok(job)
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut messages: Vec<Value> = c
            .prepare(
                "SELECT ts,sender,text,meta_json IS NOT NULL FROM messages WHERE workspace||':'||channel||':'||root_ts=?
                 ORDER BY CAST(ts AS REAL) DESC,id DESC LIMIT 2",
            )?
            .query_map([&id], |r| {
                Ok(json!({"ts":r.get::<_,String>(0)?,"sender":r.get::<_,String>(1)?,
                    "text":cut(&r.get::<_,String>(2)?,300),"from_agent":r.get::<_,bool>(3)?}))
            })?
            .collect::<rusqlite::Result<_>>()?;
        messages.reverse();
        let root: String = c
            .query_row(
                "SELECT text FROM messages WHERE workspace||':'||channel||':'||root_ts=? AND ts=root_ts LIMIT 1",
                [&id],
                |r| r.get(0),
            )
            .unwrap_or_default();
        let items: Vec<&str> = items.split(',').filter(|s| !s.is_empty()).collect();
        views.push(json!({
            "thread":root_ts,"status":status,"root":cut(&root,300),
            "linked_by":{"shared_items":items,"this_thread_refers_to_it":referenced,"it_refers_to_this_thread":references_this},
            "summary":cut(&summary,600),"notes":cut(&notes.to_string(),600),"decisions":decisions,
            "open_asks":asks,"jobs":jobs,"latest_messages":messages,
        }));
    }
    Ok(views)
}
