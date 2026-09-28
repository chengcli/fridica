//! Read attachment context after triage, with durable snapshots reused for repair.
//! This decorator holds the Slack adapter; the model/worker adapters never do.
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        parent::{Parent, ParentFailure, ParentRequest},
        time::Clock,
    },
    slack::files::{self, Downloader},
    store::Store,
};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, sync::Arc, time::Duration};

pub struct WithAttachments<P: Parent, D: Downloader> {
    parent: Arc<P>,
    files: Arc<D>,
    config: Arc<Config>,
    store: Store,
    clock: Arc<dyn Clock>,
    timeout: Duration,
}
fn failure(code: &str) -> ParentFailure {
    ParentFailure { code: code.into() }
}
fn strip(message: &mut Value) {
    if let Some(m) = message.as_object_mut() {
        m.remove("attachments");
    }
}
impl<P: Parent, D: Downloader> WithAttachments<P, D> {
    pub fn new(
        parent: Arc<P>,
        files: Arc<D>,
        config: Arc<Config>,
        store: Store,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            parent,
            files,
            config,
            store,
            clock,
            timeout: Duration::from_secs(30),
        }
    }
    async fn prepare(&self, mut request: ParentRequest) -> Result<ParentRequest, ParentFailure> {
        // No file I/O for triage, worker-result summaries or owner control calls.
        if !matches!(request.call.as_str(), "decide" | "repair")
            || request.trigger["kind"] != "message"
        {
            if let Some(message) = request.trigger.get_mut("message") {
                strip(message);
            }
            for message in &mut request.history {
                strip(message);
            }
            return Ok(request);
        }
        if request.session["workspace"] != self.config.slack.workspace
            || !request.session["channel"]
                .as_str()
                .is_some_and(|c| self.config.slack.channels.iter().any(|v| v == c))
        {
            return Err(failure("parent_context_scope"));
        }
        let mut messages = vec![request.trigger["message"].clone()];
        messages.extend(request.history.iter().rev().cloned());
        let input = json!({"session":request.session["id"],"version":request.session["version"],"inbox":request.inbox_id,"messages":messages});
        let key = format!("{:x}", Sha256::digest(input.to_string().as_bytes()));
        let context = if request.call == "repair" {
            let raw=self.store.call(move|c|Ok(c.query_row("SELECT json_extract(payload_json,'$.context') FROM replay_events WHERE kind='parent_attachment_result' AND complete=1 AND json_extract(payload_json,'$.key')=? ORDER BY seq DESC LIMIT 1",[key],|r|r.get::<_,String>(0)).optional()?)).await.map_err(|_|failure("parent_context_recording_failed"))?;
            serde_json::from_str::<Value>(
                &raw.ok_or_else(|| failure("parent_context_snapshot_missing"))?,
            )
            .map_err(|_| failure("parent_context_snapshot_invalid"))?
        } else {
            let owner = self.config.owner.slack_user.clone();
            let channel = request.session["channel"].as_str().unwrap().to_owned();
            let root = request.session["root_ts"].as_str().unwrap_or("").to_owned();
            let candidates: Vec<(String, String)> = messages
                .iter()
                .filter(|m| m["sender"] == owner)
                .flat_map(|m| m["attachments"].as_array().into_iter().flatten())
                .filter_map(|a| {
                    Some((a["id"].as_str()?.to_owned(), a["name"].as_str()?.to_owned()))
                })
                .collect();
            let now = self.clock.now();
            if !now.is_finite() {
                return Err(failure("parent_context_time"));
            }
            let call_key = key.clone();
            // Own-file classification and intent share a database snapshot.
            let (call,own)=self.store.call(move|c|{
                let tx=c.transaction()?;let mut own=BTreeSet::new();
                for (id,name) in candidates {
                    let ours:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox WHERE kind='upload' AND (sent_ts=? OR ((sent_ts IS NULL OR sent_ts='') AND channel=? AND thread_ts=? AND filename=?)))",params![id,channel,root,name],|r|r.get(0))?;
                    if ours{own.insert(id);}
                }
                tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_attachment_call',?,?,0)",params![now,json!({"key":call_key,"source":input,"own":own}).to_string()])?;
                let call=tx.last_insert_rowid();tx.commit()?;Ok((call,own))
            }).await.map_err(|_|failure("parent_context_recording_failed"))?;
            let mut views = files::read(self.files.as_ref(), &messages, &own, self.timeout)
                .await
                .map_err(|_| failure("parent_context_recording_failed"))?;
            let mut trigger = request.trigger.clone();
            let mut history = request.history.clone();
            let trigger_id = trigger["message"]["event_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            strip(&mut trigger["message"]);
            if let Some(attached) = views.remove(&trigger_id) {
                trigger["message"]["attachments"] = json!(attached);
            }
            for message in &mut history {
                strip(message);
                if let Some(id) = message["event_id"].as_str() {
                    if let Some(attached) = views.get(id) {
                        message["attachments"] = json!(attached);
                    }
                }
            }
            // Persist exactly the rendered attachment snapshot. The parent's
            // existing context budget applies after this step and is recorded
            // with its transport prompt. Never replace the raw stored messages.
            let context = json!({"trigger":trigger,"history":history});
            let record = json!({"call":call,"key":key,"context":context});
            let now = self.clock.now();
            self.store.call(move|c|{let tx=c.transaction()?;
                tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('parent_attachment_result',?,?)",params![now,record.to_string()])?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?",[call])?;tx.commit()?;Ok(())
            }).await.map_err(|_|failure("parent_context_recording_failed"))?;
            context
        };
        request.trigger = context["trigger"].clone();
        request.history = serde_json::from_value(context["history"].clone())
            .map_err(|_| failure("parent_context_snapshot_invalid"))?;
        Ok(request)
    }
}
impl<P: Parent, D: Downloader> Parent for WithAttachments<P, D> {
    fn preparation_timeout(&self, request: &ParentRequest) -> Duration {
        if request.call == "decide" && request.trigger["kind"] == "message" {
            self.timeout
        } else {
            Duration::ZERO
        }
    }
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async move {
            let request = self.prepare(request).await?;
            let timeout = Duration::try_from_secs_f64(self.config.parent.timeout)
                .map_err(|_| failure("parent_invalid_timeout"))?;
            tokio::time::timeout(timeout, self.parent.decide(request))
                .await
                .map_err(|_| failure("parent_timeout"))?
        })
    }
}
