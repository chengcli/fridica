//! Read-only Slack history paging and durable catch-up watermarks. The injected
//! transport must authenticate requests, disable automatic retries, and bound
//! response bytes before decoding. No network client or scheduler lives here.
use super::{
    ingress::{normalize, timestamp, ENVELOPE_LIMIT},
    receiver::Receiver,
};
use crate::attention;
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

pub const WINDOW: f64 = 3600.;
pub const RECENT: f64 = 900.;
pub const MAX_WINDOW: f64 = 7. * 86400.;
const OVERLAP: f64 = 60.;
const DAY: f64 = 86400.;
const PAGES: usize = 10;
const CORPUS_LIMIT: usize = 32 * 1024 * 1024;

pub use fridica_slack::history::{History, HistoryFailure, Method, PageRequest};
#[derive(Debug, Default, PartialEq)]
pub struct Progress {
    pub added: usize,
    pub incomplete_channels: Vec<String>,
}
pub struct Catchup<H: History> {
    receiver: Receiver,
    history: Arc<H>,
    pass: Arc<Mutex<()>>,
    timeout: Duration,
}
impl<H: History> Clone for Catchup<H> {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
            history: self.history.clone(),
            pass: self.pass.clone(),
            timeout: self.timeout,
        }
    }
}
struct Start {
    oldest: f64,
    roots: Vec<String>,
    mark: Option<f64>,
}
impl<H: History> Catchup<H> {
    pub fn new(receiver: Receiver, history: Arc<H>, timeout: Duration) -> Result<Self> {
        if timeout.is_zero() {
            bail!("history timeout must be positive");
        }
        Ok(Self {
            receiver,
            history,
            pass: Arc::new(Mutex::new(())),
            timeout,
        })
    }
    /// Keep one service per daemon. Clones serialize passes. `started_at` is the
    /// daemon start, so a new live event cannot hide the pre-start outage window.
    pub async fn run(&self, window: f64, started_at: Option<f64>) -> Result<Progress> {
        let _guard = self.pass.lock().await;
        let now = self.receiver.clock.now();
        if !now.is_finite()
            || !window.is_finite()
            || window <= 0.
            || started_at.is_some_and(|s| !s.is_finite())
        {
            bail!("invalid catch-up window");
        }
        let mut progress = Progress::default();
        for channel in &self.receiver.config.slack.channels {
            let start = self.start(channel, window, now, started_at).await?;
            let (payloads, complete, gaps) = self.recent(channel, &start).await?;
            let mut prepared = vec![];
            let mut dropped = vec![];
            let mut first_read = now;
            for payload in payloads {
                if !self.receiver.scoped(&payload) {
                    continue;
                }
                first_read = first_read.min(
                    payload["event"]["ts"]
                        .as_str()
                        .context("missing catch-up timestamp")?
                        .parse::<f64>()?,
                );
                if let Some(message) = normalize(&payload, "catchup") {
                    prepared.push((message, self.receiver.ids.next("obligation")));
                } else if payload["event"]["text"].as_str().is_some_and(|text| {
                    text.contains(&format!("<@{}>", self.receiver.config.owner.slack_user))
                }) {
                    dropped.push(json!({"event_id":payload["event_id"],"reason":"unsupported_message","source":"catchup"}));
                }
            }
            let config = self.receiver.config.clone();
            let channel = channel.clone();
            let incomplete_channel = channel.clone();
            let added = self.receiver.store.call(move |c| {
                let tx = c.transaction()?;
                let mut added = 0;
                for (message,id) in prepared {
                    if attention::intake_tx(&tx,message,&config.owner.slack_user,now,config.attention.mention_grace,&id,Some(now-DAY))?.is_some() { added+=1; }
                }
                // A root Slack refuses (e.g. deleted) is skipped, once per root,
                // so it cannot stall the watermark.
                for (root, code) in gaps {
                    let detail = json!({"workspace":config.slack.workspace,"channel":channel,"root":root,"code":code});
                    tx.execute("INSERT INTO health_events(kind,details_json,created) SELECT 'slack_catchup_skipped_thread',?,? WHERE NOT EXISTS(SELECT 1 FROM health_events WHERE kind='slack_catchup_skipped_thread' AND json_extract(details_json,'$.channel')=? AND json_extract(details_json,'$.root')=?)",params![detail.to_string(),now,channel,root])?;
                }
                for detail in dropped {
                    tx.execute("INSERT INTO health_events(kind,details_json,created) SELECT 'slack_dropped_mention',?,? WHERE NOT EXISTS(SELECT 1 FROM health_events WHERE kind='slack_dropped_mention' AND json_extract(details_json,'$.event_id')=?)",params![detail.to_string(),now,detail["event_id"].as_str()])?;
                }
                let workspace = &config.slack.workspace;
                let key = format!("catchup:{workspace}:{channel}");
                let runs_key = format!("{key}:truncated");
                let current: Option<f64> = tx.query_row("SELECT last_complete_pass FROM channel_watermarks WHERE workspace=? AND channel=?",params![workspace,channel],|r|r.get(0)).optional()?;
                // Fencing a separately constructed service prevents an older pass
                // from overwriting another pass's committed watermark.
                if current!=start.mark { bail!("catch-up watermark changed during pass"); }
                let runs: Option<String> = tx.query_row("SELECT value FROM meta WHERE key=?",[&runs_key],|r|r.get(0)).optional()?;
                let runs = runs.map(|s|s.parse::<u32>()).transpose()?.unwrap_or(0).saturating_add(1);
                let mark = if complete || runs>=2 { now } else { current.unwrap_or(start.oldest+OVERLAP) };
                let pinned = !complete && runs<2;
                tx.execute("INSERT INTO channel_watermarks VALUES(?,?,?,?) ON CONFLICT(workspace,channel) DO UPDATE SET last_complete_pass=excluded.last_complete_pass,pinned=excluded.pinned",params![workspace,channel,mark,pinned])?;
                tx.execute("INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,format!("{mark:.6}")])?;
                tx.execute("INSERT INTO meta VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![runs_key,if pinned {runs.to_string()} else {"0".into()}])?;
                if !complete && runs>=2 {
                    tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('slack_catchup_gap',?,?)",params![json!({"workspace":workspace,"channel":channel,"oldest":start.oldest,"before":first_read,"passes":runs}).to_string(),now])?;
                }
                tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('slack_catchup_commit',?,?)",params![now,json!({"workspace":workspace,"channel":channel,"oldest":start.oldest,"complete":complete,"mark":mark,"pinned":pinned,"added":added}).to_string()])?;
                tx.commit()?;
                Ok(added)
            }).await?;
            progress.added += added;
            if !complete {
                progress.incomplete_channels.push(incomplete_channel);
            }
        }
        Ok(progress)
    }
    async fn start(
        &self,
        channel: &str,
        window: f64,
        now: f64,
        started: Option<f64>,
    ) -> Result<Start> {
        let workspace = self.receiver.config.slack.workspace.clone();
        let channel = channel.to_string();
        self.receiver.store.call(move |c| {
            let tx=c.transaction()?;
            let mark: Option<f64> = tx.query_row("SELECT last_complete_pass FROM channel_watermarks WHERE workspace=? AND channel=?",params![workspace,channel],|r|r.get(0)).optional()?;
            let latest: Option<f64> = tx.query_row("SELECT MAX(CAST(ts AS REAL)) FROM messages WHERE workspace=? AND channel=? AND (? IS NULL OR received_at<?)",params![workspace,channel,started,started],|r|r.get(0))?;
            let oldest = (now-window).min(mark.or(latest).map(|s|s-OVERLAP).unwrap_or(now-window)).max(now-MAX_WINDOW);
            if !oldest.is_finite() { bail!("invalid stored watermark"); }
            // Match Python's updated-order limit before filtering by age.
            let roots = tx.prepare("SELECT root_ts FROM (SELECT root_ts,updated FROM threads WHERE workspace=? AND channel=? ORDER BY updated DESC LIMIT 200) WHERE updated>=?")?
                .query_map(params![workspace,channel,oldest-DAY],|r|r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
            tx.commit()?;
            Ok(Start {oldest,roots,mark})
        }).await
    }
    /// Messages since `start`, whether every page was read, and the thread
    /// roots Slack refused (root, error code).
    async fn recent(
        &self,
        channel: &str,
        start: &Start,
    ) -> Result<(Vec<Value>, bool, Vec<(String, String)>)> {
        let mut size = 0;
        let mut gaps = vec![];
        let (items, mut complete, _) = self.pages(channel, start.oldest, None, &mut size).await?;
        let mut found: BTreeMap<String, Value> = items
            .into_iter()
            .filter_map(|v| Some((v["ts"].as_str()?.into(), v)))
            .collect();
        let mut roots: BTreeSet<String> = start.roots.iter().cloned().collect();
        roots.extend(
            found
                .iter()
                .filter(|(_, v)| v["reply_count"].as_u64().is_some_and(|n| n > 0))
                .map(|(ts, _)| ts.clone()),
        );
        for root in roots {
            if !timestamp(&root) {
                continue;
            }
            let (items, whole, refused) = self
                .pages(channel, start.oldest, Some(root.clone()), &mut size)
                .await?;
            complete &= whole;
            if let Some(code) = refused {
                gaps.push((root.clone(), code));
            }
            for item in items {
                let ts = item["ts"]
                    .as_str()
                    .context("missing history timestamp")?
                    .to_owned();
                if ts != root {
                    found.entry(ts).or_insert(item);
                }
            }
        }
        let mut payloads = vec![];
        for (ts, mut item) in found {
            if !timestamp(&ts) || ts.parse::<f64>()? < start.oldest {
                continue;
            }
            item["type"] = json!("message");
            item["channel"] = json!(channel);
            payloads.push(json!({"type":"event_callback","event_id":format!("catchup:{channel}:{ts}"),"team_id":self.receiver.config.slack.workspace,"event":item}));
        }
        Ok((payloads, complete, gaps))
    }
    async fn pages(
        &self,
        channel: &str,
        oldest: f64,
        root: Option<String>,
        size: &mut usize,
    ) -> Result<(Vec<Value>, bool, Option<String>)> {
        let mut items = vec![];
        let mut cursor = None;
        for _ in 0..PAGES {
            let request = PageRequest {
                method: if root.is_some() {
                    Method::Replies
                } else {
                    Method::History
                },
                channel: channel.into(),
                oldest: format!("{oldest:.6}"),
                ts: root.clone(),
                cursor,
                limit: 200,
                include_all_metadata: true,
            };
            let response = match self.page(request).await? {
                Ok(response) => response,
                // One refused thread (deleted, or otherwise unreadable) is a gap
                // in that thread only; a refused channel history fails the pass.
                Err(code) if root.is_some() => return Ok((items, true, Some(code))),
                Err(code) => bail!("Slack history rejected request ({code}); watermark unchanged"),
            };
            *size += serde_json::to_vec(&response)?.len();
            if *size > CORPUS_LIMIT {
                bail!("catch-up response budget exceeded; watermark unchanged");
            }
            let messages = response["messages"]
                .as_array()
                .context("history response lacks messages")?;
            items.extend(
                messages
                    .iter()
                    .filter(|v| v.is_object() && v["ts"].is_string())
                    .cloned(),
            );
            cursor = match &response["response_metadata"]["next_cursor"] {
                Value::Null => None,
                Value::String(s) if s.is_empty() => None,
                Value::String(s) if s.len() <= 4096 => Some(s.clone()),
                _ => bail!("invalid history cursor"),
            };
            if cursor.is_none() {
                // A has_more response without its cursor cannot certify a full pass.
                return Ok((items, response["has_more"] != true, None));
            }
        }
        Ok((items, false, None))
    }
    /// One recorded page: Slack's response, or the error code of a refusal.
    /// Timeouts, transport and recording failures are errors.
    async fn page(&self, request: PageRequest) -> Result<std::result::Result<Value, String>> {
        let now = self.receiver.clock.now();
        let record = serde_json::to_string(&request)?;
        let call=self.receiver.store.call(move |c| {
            c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_history_call',?,?,0)",params![now,record])?;
            Ok(c.last_insert_rowid())
        }).await?;
        let mut result = tokio::time::timeout(self.timeout, self.history.page(request))
            .await
            .unwrap_or(Err(HistoryFailure::Timeout));
        let mut complete = true;
        if result
            .as_ref()
            .is_ok_and(|v| serde_json::to_vec(v).map_or(true, |bytes| bytes.len() > ENVELOPE_LIMIT))
        {
            result = Err(HistoryFailure::InvalidResponse);
            complete = false; // Never label a discarded oversized boundary exact replay.
        }
        let now = self.receiver.clock.now();
        let record = json!({"call":call,"result":result});
        let record = if self.receiver.config.state.record == "full" {
            record
        } else {
            crate::store::record::history(record)
        };
        self.receiver.store.call(move |c| {
            let tx=c.transaction()?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('slack_history_result',?,?,?)",params![now,record.to_string(),complete])?;
            tx.execute("UPDATE replay_events SET complete=? WHERE seq=?",params![complete,call])?;
            tx.commit()?; Ok(())
        }).await?;
        let response = match result {
            Ok(response) => response,
            Err(HistoryFailure::Rejected { code }) => return Ok(Err(code)),
            Err(error) => return Err(anyhow::Error::new(error)),
        };
        if response["ok"] != true {
            let code = response["error"].as_str().unwrap_or("rejected");
            return Ok(Err(code.chars().take(64).collect()));
        }
        Ok(Ok(response))
    }
}
