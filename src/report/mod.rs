//! Reports are collected inside one SQLite snapshot. Export intent commits with
//! the report; retries may replace files but never create additional channel posts.
use crate::store::{migration::atomic_write, Store};
use anyhow::{bail, Context, Result};
use chrono::{Days, LocalResult, NaiveDate, TimeZone};
use chrono_tz::Tz;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize, Deserialize)]
pub struct Daily {
    pub channel: String,
    pub day: String,
    pub timezone: String,
    pub start: f64,
    pub end: f64,
    pub messages: i64,
    pub mentions: i64,
    pub replies_delivered: i64,
    pub replies_ambiguous: i64,
    pub obligations_open: i64,
    pub obligations_answered: i64,
    pub jobs_finished: i64,
    pub health_events: i64,
    pub campaign_items: i64,
}

pub fn day_bounds(day: NaiveDate, zone: Tz) -> Result<(f64, f64)> {
    fn boundary(day: NaiveDate, zone: Tz) -> Result<f64> {
        // Some zones move clocks at midnight. Find the first valid instant of
        // the local date; choose the earlier occurrence when midnight repeats.
        for minute in 0..1440 {
            let time = day.and_hms_opt(minute / 60, minute % 60, 0).unwrap();
            let value = match zone.from_local_datetime(&time) {
                LocalResult::Single(v) => Some(v),
                LocalResult::Ambiguous(a, b) => Some(a.min(b)),
                LocalResult::None => None,
            };
            if let Some(v) = value {
                return Ok(v.timestamp() as f64);
            }
        }
        bail!("local date does not exist in timezone")
    }
    Ok((
        boundary(day, zone)?,
        boundary(
            day.checked_add_days(Days::new(1))
                .context("date overflow")?,
            zone,
        )?,
    ))
}

pub async fn generate(
    store: &Store,
    channel: String,
    day: NaiveDate,
    zone: Tz,
    now: f64,
) -> Result<Daily> {
    if channel.is_empty()
        || !channel
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        bail!("invalid report channel");
    }
    let (start, end) = day_bounds(day, zone)?;
    store.call(move|c|{
        let tx=c.transaction()?;
        let count=|sql:&str|->Result<i64>{Ok(tx.query_row(sql,params![channel,start,end],|r|r.get(0))?)};
        let report=Daily{channel:channel.clone(),day:day.to_string(),timezone:zone.name().into(),start,end,
            messages:count("SELECT count(*) FROM messages WHERE channel=? AND received_at>=? AND received_at<? AND source!='self'")?,
            mentions:count("SELECT count(*) FROM messages WHERE channel=? AND received_at>=? AND received_at<? AND mentions_owner=1")?,
            replies_delivered:count("SELECT count(*) FROM outbox WHERE channel=? AND delivered_at>=? AND delivered_at<? AND state='sent' AND kind='reply'")?,
            replies_ambiguous:count("SELECT count(*) FROM outbox WHERE channel=? AND created>=? AND created<? AND state='ambiguous'")?,
            obligations_open:count("SELECT count(*) FROM obligations o JOIN threads t ON t.id=o.session_id WHERE t.channel=? AND o.created>=? AND o.created<? AND o.state IN ('open','deferred','awaiting_delivery')")?,
            obligations_answered:count("SELECT count(*) FROM obligations o JOIN threads t ON t.id=o.session_id WHERE t.channel=? AND o.updated>=? AND o.updated<? AND o.state='answered'")?,
            jobs_finished:count("SELECT count(*) FROM jobs j JOIN threads t ON t.id=j.session_id WHERE t.channel=? AND j.finished_at>=? AND j.finished_at<? AND j.status IN ('done','failed','interrupted','cancelled')")?,
            health_events:tx.query_row("SELECT count(*) FROM health_events WHERE created>=? AND created<?",params![start,end],|r|r.get(0))?,
            campaign_items:tx.query_row("SELECT count(*) FROM work_items WHERE updated>=? AND updated<?",params![start,end],|r|r.get(0))?,
        };
        let markdown=format!("# {} — {}\n\nTimezone: {}\n\n- Messages: {}\n- Mentions: {}\n- Replies delivered: {}\n- Ambiguous replies: {}\n- New obligations still open: {}\n- Obligations answered: {}\n- Jobs finished: {}\n- Health events (daemon): {}\n- Campaign items updated (daemon): {}\n",
            report.channel,report.day,report.timezone,report.messages,report.mentions,report.replies_delivered,report.replies_ambiguous,report.obligations_open,report.obligations_answered,report.jobs_finished,report.health_events,report.campaign_items);
        tx.execute("INSERT INTO reports(channel,day,timezone,data_json,markdown,created) VALUES(?,?,?,?,?,?)
             ON CONFLICT(channel,day) DO UPDATE SET timezone=excluded.timezone,data_json=excluded.data_json,markdown=excluded.markdown,created=excluded.created",
            params![channel,report.day,report.timezone,serde_json::to_string(&report)?,markdown,now])?;
        tx.execute("INSERT INTO report_exports(channel,day) VALUES(?,?) ON CONFLICT(channel,day) DO UPDATE SET generation=generation+1,state='pending',error=''",params![channel,report.day])?;
        tx.commit()?;Ok(report)
    }).await
}

pub async fn export_pending(store: &Store, directory: PathBuf) -> Result<usize> {
    std::fs::create_dir_all(&directory)?;
    let rows:Vec<(String,String,String,i64)>=store.call(|c|Ok(c.prepare("SELECT r.channel,r.day,r.markdown,e.generation FROM reports r JOIN report_exports e USING(channel,day) WHERE e.state='pending' ORDER BY r.day,r.channel")?
        .query_map([],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?)).await?;
    let mut count = 0;
    for (channel, day, markdown, generation) in rows {
        if !channel
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            || NaiveDate::parse_from_str(&day, "%Y-%m-%d").is_err()
        {
            bail!("invalid report export path");
        }
        let target = directory.join(format!("{day}-{channel}.md"));
        count += store.call(move |c| {
            let tx=c.transaction()?;
            let current:Option<i64>=tx.query_row("SELECT generation FROM report_exports WHERE channel=? AND day=? AND state='pending'",params![channel,day],|r|r.get(0)).optional()?;
            if current!=Some(generation) {return Ok(0);}
            // Serialize export and acknowledgement so an older export cannot
            // overwrite a newer generation after it has been acknowledged.
            atomic_write(&target,markdown.as_bytes())?;
            let count=tx.execute("UPDATE report_exports SET state='done' WHERE channel=? AND day=? AND generation=?",params![channel,day,generation])?;
            tx.commit()?;Ok(count)
        }).await?;
    }
    Ok(count)
}

pub async fn queue_post(
    store: &Store,
    channel: String,
    day: String,
    session: String,
    now: f64,
) -> Result<bool> {
    store.call(move|c|{
        let tx=c.transaction()?;
        let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM report_posts WHERE channel=? AND day=?)",params![channel,day],|r|r.get(0))?;
        if exists {return Ok(false);}
        let markdown:String=tx.query_row("SELECT markdown FROM reports WHERE channel=? AND day=?",params![channel,day],|r|r.get(0))?;
        tx.execute("INSERT INTO outbox(idem_key,session_id,kind,channel,text,created) VALUES(?,?,'report',?,?,?)",params![format!("daily:{channel}:{day}"),session,channel,markdown,now])?;
        tx.execute("INSERT INTO report_posts VALUES(?,?,?)",params![channel,day,tx.last_insert_rowid()])?;
        tx.commit()?;Ok(true)
    }).await
}
