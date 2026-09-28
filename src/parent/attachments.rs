//! Read attachment and optional linked context after triage; reuse snapshots for repair.
//! This decorator holds the Slack adapter; the model/worker adapters never do.
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        parent::{Parent, ParentFailure, ParentRequest},
        time::Clock,
    },
    slack::{
        files::{self, Downloader},
        links,
    },
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
    links: Option<Arc<dyn links::Reader>>,
    github: Option<Arc<dyn crate::github::links::Reader>>,
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
            links: None,
            github: None,
        }
    }
    /// Add linked messages to the same preparation budget and durable snapshot.
    pub fn with_links(mut self, reader: Arc<dyn links::Reader>) -> Self {
        self.links = Some(reader);
        self
    }
    pub fn with_github(mut self, reader: Arc<dyn crate::github::links::Reader>) -> Self {
        self.github = Some(reader);
        self
    }
    async fn prepare(&self, mut request: ParentRequest) -> Result<ParentRequest, ParentFailure> {
        request.linked.clear();
        request.github_state.clear();
        let message_call = request.trigger["kind"] == "message";
        let github_enabled = self.config.github.enabled && self.github.is_some();
        // Triage never performs external reads. GitHub context also informs
        // worker-result summaries and authenticated instruction calls.
        if !matches!(request.call.as_str(), "decide" | "repair")
            || (!message_call && !github_enabled)
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
        let mut messages = if message_call {
            vec![request.trigger["message"].clone()]
        } else {
            vec![json!({"text":request.trigger["text"]})]
        };
        messages.extend(request.history.iter().rev().cloned());
        let input = json!({"session":request.session["id"],"version":request.session["version"],"inbox":request.inbox_id,"messages":messages,"links":self.links.is_some(),"context_version":3,"github":github_enabled,"github_cache_seconds":self.config.github.cache_seconds,"scope":{"workspace":self.config.slack.workspace,"channels":self.config.slack.channels,"owner":self.config.owner.slack_user}});
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
            // Independent reads share the actor's 30-second allowance, so neither
            // consumes the model deadline nor doubles the preparation budget.
            let linked = async {
                match &self.links {
                    Some(reader) if message_call => {
                        links::read(
                            reader.as_ref(),
                            &self.config.slack.channels,
                            request.session["channel"].as_str().unwrap(),
                            request.session["root_ts"].as_str().unwrap_or(""),
                            &messages,
                            self.timeout,
                        )
                        .await
                    }
                    _ => Ok(vec![]),
                }
            };
            let attachments = async {
                if message_call {
                    files::read(self.files.as_ref(), &messages, &own, self.timeout).await
                } else {
                    Ok(Default::default())
                }
            };
            let github = async {
                match &self.github {
                    Some(reader) if github_enabled => {
                        let texts = messages
                            .iter()
                            .filter_map(|m| m["text"].as_str().map(str::to_owned))
                            .collect();
                        match tokio::time::timeout(Duration::from_secs(20), reader.linked(texts))
                            .await
                        {
                            Ok(Ok(data)) => Ok((data, "complete")),
                            Ok(Err(crate::github::client::Failure::Recording)) => {
                                Err(failure("parent_context_recording_failed"))
                            }
                            Ok(Err(_)) => Ok((vec![], "unavailable")),
                            Err(_) => Ok((vec![], "timeout")),
                        }
                    }
                    _ => Ok((vec![], "disabled")),
                }
            };
            let (views, linked, github) = tokio::join!(attachments, linked, github);
            let mut views = views.map_err(|_| failure("parent_context_recording_failed"))?;
            let linked = linked.map_err(|_| failure("parent_context_recording_failed"))?;
            let (github_state, github_status) = github?;
            let mut trigger = request.trigger.clone();
            let mut history = request.history.clone();
            let trigger_id = trigger["message"]["event_id"]
                .as_str()
                .unwrap_or("")
                .to_owned();
            if let Some(message) = trigger.get_mut("message") {
                strip(message);
            }
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
            // Persist the rendered attachment and linked-message snapshot.
            // Attachment budgets also apply when building the transport prompt;
            // linked text has its own budget. Never replace raw stored messages.
            let context = json!({"trigger":trigger,"history":history,"linked":linked,"github_state":github_state,"github_status":github_status});
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
        request.linked = serde_json::from_value(context["linked"].clone())
            .map_err(|_| failure("parent_context_snapshot_invalid"))?;
        request.github_state = serde_json::from_value(context["github_state"].clone())
            .map_err(|_| failure("parent_context_snapshot_invalid"))?;
        Ok(request)
    }
}
impl<P: Parent, D: Downloader> Parent for WithAttachments<P, D> {
    fn preparation_timeout(&self, request: &ParentRequest) -> Duration {
        if request.call == "decide"
            && (request.trigger["kind"] == "message"
                || (self.config.github.enabled && self.github.is_some()))
        {
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
