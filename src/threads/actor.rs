//! Durable message, owner-instruction, obligation-due and worker-result turns.
//! Parent I/O never holds a database transaction. Version checks fence owner
//! controls arriving during a call; all resulting state effects commit together.
use crate::{
    attention::{self, Answer, Capacity},
    config::{Attention, Config},
    core::{
        parent::{Decision, Disposition, NoteKind, Parent, ParentRequest, ReplyStatus},
        policy::{attention_gate, GateInput},
        time::{Clock, Identifiers},
    },
    store::{work, Store},
};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::params;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc, time::Duration};

#[derive(Debug, PartialEq)]
pub enum Step {
    Idle,
    Observed,
    Deferred,
    Committed,
    Stale,
    Failed,
    Unsupported,
}

pub struct Actor<P: Parent> {
    /// None supports reply-only operation; delegation requires a validated config.
    pub config: Option<Arc<Config>>,
    pub store: Store,
    pub parent: Arc<P>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn Identifiers>,
    pub owner: String,
    pub limits: Attention,
    pub observe_only: bool,
    pub parent_timeout: Duration,
}

impl<P: Parent> Actor<P> {
    pub async fn step(&self, session: String) -> Result<Step> {
        if !self.observe_only {
            let pending_session = session.clone();
            if self
                .store
                .call(move |c| crate::store::worker_controls::pending_tx(c, &pending_session))
                .await?
            {
                return Ok(Step::Deferred);
            }
        }
        let Some((id, kind)) =
            attention::claim_due(&self.store, session.clone(), self.clock.now()).await?
        else {
            return Ok(Step::Idle);
        };
        match self.handle(id, &kind, session.clone()).await {
            Ok(step) => Ok(step),
            Err(_error) => {
                let now = self.clock.now();
                let recorded = self.store.call(move|c| {
                    let tx=c.transaction()?;
                    let (attempts,state):(i64,String)=tx.query_row("SELECT attempts,state FROM thread_inbox WHERE id=?",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
                    // Cleaning may discard this turn while an external call is
                    // failing. Its late error must not create fresh signal work.
                    if state != "processing" { return Ok(false); }
                    tx.execute("UPDATE thread_inbox SET attempts=attempts+1,state=?,not_before=? WHERE id=? AND state='processing'",params![if attempts>=2{"dropped"}else{"pending"},now+30.,id])?;
                    tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
                    tx.execute("INSERT OR IGNORE INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated)
                        SELECT ?,?,'signal',?,?,'An inbox turn failed; review required',?,?,?
                        WHERE NOT EXISTS(SELECT 1 FROM thread_inbox i JOIN obligations o ON i.ref=o.id WHERE i.id=? AND i.kind='obligation_due' AND o.kind='signal')",params![format!("inbox-failed:{id}"),session,format!("inbox-failed:{id}"),json!({"inbox_id":id}).to_string(),now,now,now,id])?;
                    tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'system','inbox.failed',?,?)",params![now,session,json!({"inbox_id":id,"attempt":attempts+1}).to_string()])?;
                    tx.commit()?;Ok(true)
                }).await?;
                Ok(if recorded { Step::Failed } else { Step::Stale })
            }
        }
    }

    async fn handle(&self, id: i64, kind: &str, session: String) -> Result<Step> {
        let mut request = load(&self.store, id, session.clone()).await?;
        if let Some(config) = &self.config {
            let busy = serde_json::from_value(request.session["work"]["busy"].clone())?;
            request.session["machines"] = json!(config.machines.payload(&busy));
        }
        let worker_result = matches!(kind, "worker_result" | "worker_interrupted");
        if self.observe_only || request.session["control"] != "active" {
            let reason = if self.observe_only {
                "observe-only mode"
            } else {
                "thread is paused or closed"
            };
            return settle(
                &self.store,
                id,
                &request,
                format!("observe: {reason}"),
                worker_result.then(|| self.clock.now() + 60.),
            )
            .await;
        }
        if kind == "debrief" {
            return super::debrief::handle(self, request).await;
        }
        if worker_result
            && (request.trigger["pending"] == true
                || request.trigger["results"]
                    .as_array()
                    .is_none_or(|r| r.is_empty()))
        {
            return settle(
                &self.store,
                id,
                &request,
                "observe: group still working or already reported".into(),
                None,
            )
            .await;
        }
        if !matches!(
            kind,
            "message"
                | "owner_instruction"
                | "obligation_due"
                | "worker_result"
                | "worker_interrupted"
        ) {
            let outcome = settle(
                &self.store,
                id,
                &request,
                String::new(),
                Some(self.clock.now() + 60.),
            )
            .await?;
            return Ok(if outcome == Step::Observed {
                Step::Unsupported
            } else {
                outcome
            });
        }
        if kind == "obligation_due" {
            let ref_id = request.trigger["ref"].as_str().unwrap_or("");
            if !request.obligations.iter().any(|o| {
                o["id"] == ref_id
                    && matches!(o["state"].as_str(), Some("open" | "deferred"))
                    && o["due"].as_f64().is_some_and(|due| due <= self.clock.now())
            }) {
                return settle(
                    &self.store,
                    id,
                    &request,
                    "observe: obligation no longer due".into(),
                    None,
                )
                .await;
            }
        }
        let mut turn = request.session["turns"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1);
        let mut verdict = "respond: owner instruction or attention due".to_owned();
        let mut calls = Vec::new();
        let mut unsolicited = false;
        let trigger = if worker_result {
            turn = request.session["turns"].as_u64().unwrap_or(0);
            request.trigger["origin"]["class"]
                .as_str()
                .unwrap_or("human")
        } else if kind == "owner_instruction" {
            "owner"
        } else if kind == "obligation_due" {
            if request.trigger["source_peer"] == true {
                "peer"
            } else {
                "human"
            }
        } else {
            let m = &request.trigger["message"];
            let gate = GateInput {
                owner: self.owner.clone(),
                text: m["text"].as_str().unwrap_or("").into(),
                sender: m["sender"].as_str().unwrap_or("").into(),
                generated: !m["meta"].is_null(),
                meta_kind: m["meta"]["kind"].as_str().unwrap_or("").into(),
                meta_status: m["meta"]["status"].as_str().unwrap_or("").into(),
                peer_turn: m["meta"]["turn"].as_u64().unwrap_or(0),
                control: "active".into(),
                status: request.session["status"].as_str().unwrap_or("new").into(),
                turns: request.session["turns"].as_u64().unwrap_or(0),
                reset_at: request.session["reset_at"].as_f64().unwrap_or(0.),
                ts: m["ts"].as_str().unwrap_or("0").parse()?,
                general_messages: self
                    .config
                    .as_ref()
                    .is_some_and(|c| c.slack.general_messages),
                cooling: request.session["last_unsolicited"]
                    .as_f64()
                    .is_some_and(|last| {
                        self.config
                            .as_ref()
                            .is_some_and(|c| self.clock.now() - last < c.slack.cooldown)
                    }),
                observe_only: false,
                resumed: request.trigger["payload"]["resumed"] == true,
            };
            // Addressed messages in a blocked thread reach the parent so it can
            // decide whether the new instruction actually resolves the blocker.
            let outcome = attention_gate(&gate, gate.text.contains(&format!("<@{}>", self.owner)));
            turn = outcome.turn;
            verdict = format!("{}: {}", outcome.kind, outcome.reason);
            if matches!(outcome.kind.as_str(), "ignore" | "observe") {
                return settle(&self.store, id, &request, verdict, None).await;
            }
            if outcome.kind == "triage" {
                request.call = "triage".into();
                let (raw, call) = self.call(&request).await?;
                calls.push(call);
                let choice = raw
                    .as_ref()
                    .and_then(|r| r.as_object())
                    .filter(|r| r.len() == 1)
                    .and_then(|r| r.get("decision"))
                    .and_then(Value::as_str)
                    .filter(|s| matches!(*s, "respond" | "observe" | "ignore"))
                    .unwrap_or("observe");
                if choice != "respond" {
                    return settle_triage(
                        &self.store,
                        id,
                        session,
                        request,
                        format!("{choice}: triage"),
                        calls,
                        self.clock.now(),
                    )
                    .await;
                }
                // Avoid another external call after a pause/resume changed the
                // decision context while triage was in flight.
                if !current(&self.store, id, &session, &request).await? {
                    return settle_triage(
                        &self.store,
                        id,
                        session,
                        request,
                        "observe: stale triage".into(),
                        calls,
                        self.clock.now(),
                    )
                    .await;
                }
                verdict = format!("respond: {}", outcome.reason);
                unsolicited = gate.turns == 0;
            }
            if request.trigger["payload"]["owner_trigger"] == true {
                "owner"
            } else if gate.generated {
                "peer"
            } else {
                "human"
            }
        };
        if let Capacity::Deferred(_) = attention::reserve(
            &self.store,
            session.clone(),
            id,
            trigger.into(),
            self.clock.now(),
            self.ids.next("reply"),
            self.limits.clone(),
        )
        .await?
        {
            return Ok(Step::Deferred);
        }
        let reply_limit = self.config.as_ref().map_or(7000, |c| c.limits.reply_chars);
        let mut result = if worker_result {
            super::results::direct(
                &request,
                self.config
                    .as_ref()
                    .is_some_and(|c| c.limits.report_fast_path),
                reply_limit,
            )?
        } else {
            None
        };
        for round in 0..2 {
            if result.is_some() {
                break;
            }
            request.call = if round == 0 { "decide" } else { "repair" }.into();
            let (raw, call) = self.call(&request).await?;
            let now = self.clock.now();
            let raw = raw.ok_or_else(|| anyhow!("parent call failed"))?;
            calls.push(call);
            match validate(&raw, &request, now, reply_limit).and_then(|d| {
                super::delegation::prepare(&d, &request, self.config.as_deref(), None)?;
                Ok(d)
            }) {
                Ok(decision) => {
                    result = Some(decision);
                    break;
                }
                Err(error) => {
                    request.previous = Some(raw);
                    request.errors = vec![error.to_string()];
                }
            }
        }
        let decision = result.context("parent action remained invalid after repair")?;
        let work = super::delegation::prepare(
            &decision,
            &request,
            self.config.as_deref(),
            Some(self.ids.as_ref()),
        )?;
        commit(
            &self.store,
            id,
            session,
            request,
            decision,
            work,
            calls,
            verdict,
            turn,
            &self.owner,
            self.clock.now(),
            self.limits.streak_signal,
            if unsolicited {
                self.config.as_ref().map(|c| c.slack.cooldown)
            } else {
                None
            },
        )
        .await
    }
    pub(super) async fn call(&self, request: &ParentRequest) -> Result<(Option<Value>, Value)> {
        let started = self.clock.now();
        let encoded = serde_json::to_string(request)?;
        let call_id=self.store.call(move|c| {
            c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_call',?,?,0)",params![started,encoded])?;
            Ok(c.last_insert_rowid())
        }).await?;
        let timeout = self.parent_timeout.saturating_add(
            self.parent
                .preparation_timeout(request)
                .min(Duration::from_secs(30)),
        );
        let response = tokio::time::timeout(timeout, self.parent.decide(request.clone())).await;
        let (raw, error, failure) = match response {
            Ok(Ok(value)) => (Some(value), None, None),
            Ok(Err(failure)) => (None, Some("parent_unavailable"), Some(failure)),
            Err(_) => (None, Some("parent_timeout"), None),
        };
        let now = self.clock.now();
        let outcome = json!({"call_id":call_id,"response":raw,"error":error,"failure":failure});
        self.store
            .call(move |c| {
                let tx = c.transaction()?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?", [call_id])?;
                tx.execute(
                    "INSERT INTO replay_events(kind,time,payload_json) VALUES('parent_result',?,?)",
                    params![now, outcome.to_string()],
                )?;
                tx.commit()?;
                Ok(())
            })
            .await?;
        let call =
            json!({"request":request,"response":raw,"error":error,"failure":failure,"created":now});
        Ok((raw, call))
    }
}

async fn load(store: &Store, id: i64, session: String) -> Result<ParentRequest> {
    store.call(move|c| {
        let tx=c.transaction()?;
        let data:String=tx.query_row("SELECT json_object('id',id,'workspace',workspace,'channel',channel,'root_ts',root_ts,'control',control,'status',status,'version',version,'turns',turns,'wait_streak',wait_streak,'no_progress',no_progress,'summary',summary,'last_reply_hash',last_reply_hash,'reset_at',reset_at,'context',json(context_json)) FROM threads WHERE id=?",[&session],|r|r.get(0))?;
        let (kind,reference,payload):(String,String,String)=tx.query_row("SELECT kind,ref,payload_json FROM thread_inbox WHERE id=? AND state='processing'",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let mut trigger=json!({"kind":kind,"ref":reference,"payload":serde_json::from_str::<Value>(&payload)?});
        let message_sql="SELECT json_object('event_id',event_id,'ts',ts,'sender',sender,'text',text,'files',json(files_json),'meta',json(meta_json),'attachments',json(attachments_json)) FROM messages";
        if kind=="message" {
            let raw:String=tx.query_row(&format!("{message_sql} WHERE event_id=?"),[&reference],|r|r.get(0))?;
            trigger["message"]=serde_json::from_str(&raw)?;
        }
        if kind=="obligation_due" {
            let peer:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM obligations o JOIN messages m ON m.event_id=json_extract(o.source_json,'$.event_id') WHERE o.id=? AND m.meta_json IS NOT NULL)",[&reference],|r|r.get(0))?;
            trigger["source_peer"]=json!(peer);
        }
        let history:Vec<String>=tx.prepare(&format!("{message_sql} WHERE workspace||':'||channel||':'||root_ts=? ORDER BY CAST(ts AS REAL) DESC LIMIT 60"))?
            .query_map([&session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let obligations:Vec<String>=tx.prepare("SELECT json_object('id',id,'kind',kind,'summary',summary,'due',due,'state',state,'disposition',json(state_json),'source',json(source_json),'deliveries',json((SELECT COALESCE(json_group_array(json_object('id',p.outbox_id,'state',o.state,'error',o.error)), '[]') FROM obligation_posts p JOIN outbox o ON o.id=p.outbox_id WHERE p.obligation_id=obligations.id))) FROM obligations WHERE session_id=? AND state IN ('open','deferred','awaiting_delivery') ORDER BY created,id")?
            .query_map([&session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let mut session_data:Value=serde_json::from_str(&data)?;
        let (revision, notes)=super::effects::notes(&tx,&session)?;
        session_data["notes"]=json!({"revision":revision,"data":notes});
        let (decisions,debriefed):(String,i64)=tx.query_row("SELECT decisions_json,debriefed_turn FROM threads WHERE id=?",[&session],|r|Ok((r.get(0)?,r.get(1)?)))?;
        session_data["decisions"]=serde_json::from_str(&decisions)?;
        session_data["debriefed_turn"]=json!(debriefed);
        session_data["work"]=work::context_tx(&tx,&session)?;
        let last:Option<f64>=tx.query_row("SELECT (SELECT last_unsolicited FROM cooldowns WHERE workspace=? AND channel=?)",params![session_data["workspace"].as_str(),session_data["channel"].as_str()],|r|r.get(0))?;
        session_data["last_unsolicited"]=json!(last);
        if matches!(kind.as_str(),"worker_result"|"worker_interrupted") {
            let snapshot=super::results::load(&tx,&session,&reference)?;
            for (key,value) in snapshot.as_object().context("invalid result snapshot")? {trigger[key]=value.clone();}
        }
        let result=ParentRequest{github_state:vec![],linked:vec![],inbox_id:id,call:"decide".into(),session:session_data,trigger,
            history:history.iter().rev().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,
            obligations:obligations.iter().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,previous:None,errors:vec![]};
        tx.commit()?;Ok(result)
    }).await
}

fn validate(
    raw: &Value,
    request: &ParentRequest,
    now: f64,
    reply_limit: usize,
) -> Result<Decision> {
    let mut decision: Decision =
        serde_json::from_value(raw.clone()).context("invalid parent response schema")?;
    super::effects::validate(&mut decision)?;
    if decision.reply.as_ref().is_some_and(|r| !r.send) {
        if decision
            .reply
            .as_ref()
            .is_some_and(|r| !r.answers.is_empty())
        {
            bail!("an unsent reply cannot answer obligations");
        }
        decision.reply = None;
    }
    let mut addressed = HashSet::new();
    let known: HashSet<&str> = request
        .obligations
        .iter()
        .filter(|o| matches!(o["state"].as_str(), Some("open" | "deferred")))
        .filter_map(|o| o["id"].as_str())
        .collect();
    if let Some(reply) = &decision.reply {
        if request.session["status"] == "blocked"
            && !decision.reopen_blocked
            && !matches!(reply.status, ReplyStatus::Blocked)
        {
            bail!("reopening a blocked discussion requires an explicit decision");
        }
        if reply.text.trim().is_empty()
            || reply.text.chars().count() > reply_limit
            || reply.details.chars().count() > 40000
        {
            bail!("reply or details exceed configured bounds");
        }
        if (matches!(reply.status, ReplyStatus::Blocked)
            || (request.session["status"] == "blocked" && !decision.reopen_blocked))
            && !reply.answers.is_empty()
        {
            bail!("blocked notices do not satisfy obligations");
        }
        for id in &reply.answers {
            if !known.contains(id.as_str()) || !addressed.insert(id.as_str()) {
                bail!("unknown or duplicate answer obligation");
            }
        }
    }
    for disposition in &decision.dispositions {
        if !known.contains(disposition.id()) || !addressed.insert(disposition.id()) {
            bail!("unknown or duplicate obligation disposition");
        }
        match disposition {
            Disposition::Declined { reason, .. } | Disposition::Deferred { reason, .. }
                if reason.trim().is_empty() =>
            {
                bail!("disposition needs a visible reason")
            }
            Disposition::Deferred { until, .. } if !until.is_finite() || *until <= now => {
                bail!("deferral must be in the future")
            }
            _ => {}
        }
    }
    for ask in &decision.asks {
        if ask.summary.trim().is_empty() || !ask.due.is_finite() || ask.due < now {
            bail!("invalid extracted ask");
        }
    }
    Ok(decision)
}

/// Quiet/paused decisions need the same version fence as model decisions.
/// Otherwise a late observation can consume an owner-resumed inbox item, or
/// resurrect an item which a concurrent clean deliberately dropped.
pub(super) async fn settle(
    store: &Store,
    id: i64,
    request: &ParentRequest,
    verdict: String,
    until: Option<f64>,
) -> Result<Step> {
    let session = request.session["id"]
        .as_str()
        .context("missing session ID")?
        .to_owned();
    let version = request.session["version"]
        .as_i64()
        .context("missing session version")?;
    let event = request.trigger["message"]["event_id"]
        .as_str()
        .map(str::to_owned);
    store.call(move |c| {
        let tx = c.transaction()?;
        let current: bool = tx.query_row(
            "SELECT version=? AND EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing') FROM threads WHERE id=?",
            params![version,id,session], |r| r.get(0),
        )?;
        let outcome = if !current {
            tx.execute("UPDATE thread_inbox SET state='pending' WHERE id=? AND state='processing'", [id])?;
            Step::Stale
        } else if let Some(until) = until {
            tx.execute("UPDATE thread_inbox SET state='pending',not_before=? WHERE id=?", params![until,id])?;
            Step::Observed
        } else {
            if let Some(event) = event {
                tx.execute("UPDATE messages SET verdict=? WHERE event_id=?", params![verdict,event])?;
            }
            tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?", [id])?;
            Step::Observed
        };
        tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL", [id])?;
        tx.commit()?;
        Ok(outcome)
    }).await
}

#[allow(clippy::too_many_arguments)]
async fn commit(
    store: &Store,
    id: i64,
    session: String,
    request: ParentRequest,
    decision: Decision,
    work: super::delegation::Work,
    calls: Vec<Value>,
    verdict: String,
    turn: u64,
    owner: &str,
    now: f64,
    signal: usize,
    cooldown: Option<f64>,
) -> Result<Step> {
    let owner = owner.to_owned();
    let sql_turn = i64::try_from(turn).context("turn counter exceeds supported range")?;
    store.call(move|c| {
        let tx=c.transaction()?;
        let version:i64=tx.query_row("SELECT version FROM threads WHERE id=?",[&session],|r|r.get(0))?;
        let active:bool=tx.query_row("SELECT control='active' AND EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing') FROM threads WHERE id=?",params![id,session],|r|r.get(0))?;
        let is_result=matches!(request.trigger["kind"].as_str(),Some("worker_result"|"worker_interrupted"));
        let result_stale=if is_result {super::results::load(&tx,&session,request.trigger["ref"].as_str().unwrap_or(""))?["results"]!=request.trigger["results"]} else {false};
        let notes_changed=super::effects::notes(&tx,&session)?.0 != request.session["notes"]["revision"].as_i64().unwrap_or(0);
        let controls_current=crate::store::worker_controls::current_tx(&tx,&request,&decision.worker_control)?;
        if Some(version)!=request.session["version"].as_i64() || !active || result_stale || notes_changed || !controls_current {
            tx.execute("UPDATE thread_inbox SET state='pending' WHERE id=? AND state='processing'",[id])?;
            tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
            tx.commit()?;return Ok(Step::Stale);
        }
        if let Some(cooldown)=cooldown.filter(|_|decision.reply.is_some()) {
            let last:Option<f64>=tx.query_row("SELECT (SELECT last_unsolicited FROM cooldowns WHERE workspace=? AND channel=?)",params![request.session["workspace"].as_str(),request.session["channel"].as_str()],|r|r.get(0))?;
            if let Some(last)=last.filter(|last|now-last<cooldown) {
                tx.execute("UPDATE thread_inbox SET state='pending',not_before=? WHERE id=?",params![last+cooldown,id])?;
                tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
                tx.commit()?;return Ok(Step::Deferred);
            }
        }
        let mut waiting=request.session["wait_streak"].as_i64().unwrap_or(0);
        let mut quiet=request.session["no_progress"].as_i64().unwrap_or(0)+1;
        let mut status=request.session["status"].as_str().unwrap_or("new").to_string();
        if decision.reopen_blocked && status=="blocked" {status="complete".into();}
        let mut hash=request.session["last_reply_hash"].as_str().unwrap_or("").to_owned();
        let repeat=super::replies::repeat_evidence(&tx,&request,&owner)?;
        if let Some(reply)=&decision.reply {
            let candidate=crate::core::policy::reply_hash(&reply.text);
            let explicit=repeat["allowed"]==true;
            let duplicate=candidate==hash && reply.status.as_str()==status && !explicit && reply.answers.is_empty() && reply.details.is_empty() && work.jobs.is_empty() && !is_result && decision.note.kind!=NoteKind::Correction;
            if !duplicate {
                let answer=Answer{key:format!("{id}:reply"),session:session.clone(),channel:request.session["channel"].as_str().context("missing channel")?.into(),
                    thread_ts:request.session["root_ts"].as_str().context("missing root timestamp")?.into(),text:reply.text.clone(),obligations:reply.answers.clone(),inbox:id};
                let post=attention::queue_answer_tx(&tx,&answer,now)?;
                let meta=json!({"owner":owner,"session":session,"turn":turn,"status":reply.status.as_str(),"kind":if is_result {"report"} else {"reply"},"worker":"","v":2});
                tx.execute("UPDATE outbox SET meta_json=?,trigger_event=? WHERE id=?",params![meta.to_string(),request.trigger["message"]["event_id"].as_str().or_else(||request.trigger["origin"]["event_id"].as_str()).unwrap_or(""),post])?;
                if is_result {tx.execute("UPDATE outbox SET kind='report' WHERE id=?",[post])?;}
                super::results::attachments(&tx, &request, reply, &session, id, now)?;
                if cooldown.is_some() {
                    tx.execute("INSERT OR REPLACE INTO cooldowns(workspace,channel,last_unsolicited) VALUES(?,?,?)",params![request.session["workspace"].as_str(),request.session["channel"].as_str(),now])?;
                }
                quiet=if candidate==hash || decision.note.kind==NoteKind::Ack {quiet}else{0};hash=candidate;
                waiting=if matches!(reply.status,ReplyStatus::Waiting) {waiting+1}else{0};
                status=reply.status.as_str().into();
            }
        }
        for w in &work.workers {work::add_worker_tx(&tx,w,now)?;}
        for j in &work.jobs {work::enqueue_tx(&tx,j,now)?;}
        crate::store::worker_controls::enqueue_tx(&tx,&request,&decision.worker_control,now)?;
        if is_result {
            for r in request.trigger["results"].as_array().context("missing results")? {
                tx.execute("UPDATE jobs SET reported=1 WHERE id=? AND session_id=?",params![r["id"].as_str(),session])?;
            }
        }
        let working:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM jobs WHERE session_id=? AND status IN ('queued','running'))",[&session],|r|r.get(0))?;
        if working && status!="blocked" {status="working".into();}
        if !work.jobs.is_empty() {if decision.note.kind!=NoteKind::Ack {quiet=0;}waiting=0;}
        if !work.context.is_null() {
            tx.execute("UPDATE threads SET context_json=json_patch(context_json,?) WHERE id=?",params![work.context.to_string(),session])?;
        }
        super::effects::commit(&tx,&decision,&session,id,now)?;
        for disposition in &decision.dispositions {
            let (state,due)=match disposition {Disposition::Declined{..}=>("declined",None),Disposition::Deferred{until,..}=>("deferred",Some(*until))};
            if tx.execute("UPDATE obligations SET state=?,state_json=?,due=COALESCE(?,due),updated=? WHERE id=? AND session_id=? AND state IN ('open','deferred')",
                params![state,serde_json::to_string(disposition)?,due,now,disposition.id(),session])?!=1 {bail!("obligation changed before actor commit");}
        }
        for (index,ask) in decision.asks.iter().enumerate() {
            let key=format!("ask:{id}:{index}");
            tx.execute("INSERT INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated) VALUES(?,?,'ask',?,?,?,?,?,?)",
                params![key,session,key,json!({"inbox_id":id,"event_id":request.trigger["message"]["event_id"]}).to_string(),ask.summary,now,ask.due,now])?;
        }
        if waiting==signal as i64 || quiet==signal as i64 {
            let key=format!("streak:{session}:{id}");
            tx.execute("INSERT OR IGNORE INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated) VALUES(?,?,'signal',?,?,'Conversation needs attention',?,?,?)",
                params![key,session,key,json!({"inbox_id":id}).to_string(),now,now,now])?;
        }
        for call in calls {
            tx.execute("INSERT INTO parent_turns(session_id,inbox_id,backend,call,action_json,response_json,context_json,created) VALUES(?,?,'adapter',?,?,?,?,?)",
                params![session,id,call["request"]["call"].as_str(),serde_json::to_string(&decision)?,call["response"].to_string(),call["request"].to_string(),call["created"].as_f64()])?;
        }
        tx.execute("UPDATE threads SET status=?,turns=CASE WHEN EXISTS(SELECT 1 FROM outbox WHERE idem_key=?) THEN MAX(turns,?) ELSE turns END,
            wait_streak=?,no_progress=?,last_reply_hash=?,summary=CASE WHEN ?='' THEN summary ELSE ? END,updated=?,version=version+1 WHERE id=?",
            params![status,format!("{id}:reply"),sql_turn,waiting,quiet,hash,decision.summary,decision.summary,now,session])?;
        super::debrief::enqueue(&tx,&decision,&session,id,now)?;
        if let Some(event)=request.trigger["message"]["event_id"].as_str() {tx.execute("UPDATE messages SET verdict=? WHERE event_id=?",params![verdict,event])?;}
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?",[id])?;
        tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
        tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('actor_commit',?,?)",params![now,json!({"inbox_id":id,"request":request,"decision":decision,"workers":work.workers,"jobs":work.jobs,"reply_repeat":repeat}).to_string()])?;
        tx.commit()?;Ok(Step::Committed)
    }).await
}

/// Startup recovery keeps unsent reservations reusable by the same inbox item.
pub async fn recover(store: &Store) -> Result<usize> {
    store
        .call(|c| {
            Ok(c.execute(
                "UPDATE thread_inbox SET state='pending' WHERE state='processing'",
                [],
            )?)
        })
        .await
}

async fn current(store: &Store, id: i64, session: &str, request: &ParentRequest) -> Result<bool> {
    let session = session.to_owned();
    let version = request.session["version"].as_i64();
    store.call(move|c|Ok(c.query_row("SELECT control='active' AND version=? AND EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing') FROM threads WHERE id=?",params![version,id,session],|r|r.get(0))?)).await
}
async fn settle_triage(
    store: &Store,
    id: i64,
    session: String,
    request: ParentRequest,
    verdict: String,
    calls: Vec<Value>,
    now: f64,
) -> Result<Step> {
    store.call(move|c|{
        let tx=c.transaction()?;
        let current:bool=tx.query_row("SELECT control='active' AND version=? AND EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing') FROM threads WHERE id=?",params![request.session["version"].as_i64(),id,session],|r|r.get(0))?;
        if !current {
            tx.execute("UPDATE thread_inbox SET state='pending' WHERE id=? AND state='processing'",[id])?;
            tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
            tx.commit()?;return Ok(Step::Stale);
        }
        for call in calls {
            tx.execute("INSERT INTO parent_turns(session_id,inbox_id,backend,call,action_json,response_json,context_json,created) VALUES(?,?,'adapter','triage','{}',?,?,?)",params![session,id,call["response"].to_string(),call["request"].to_string(),call["created"].as_f64()])?;
        }
        tx.execute("UPDATE messages SET verdict=? WHERE event_id=?",params![verdict,request.trigger["message"]["event_id"].as_str()])?;
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?",[id])?;
        tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
        tx.execute("UPDATE threads SET version=version+1,updated=? WHERE id=?",params![now,session])?;
        tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('triage_commit',?,?)",params![now,json!({"inbox_id":id,"verdict":verdict}).to_string()])?;
        tx.commit()?;Ok(Step::Observed)
    }).await
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::{
        attention::Message,
        core::Authority,
        threads::controls::{self, Control},
    };

    #[tokio::test]
    async fn stale_observation_or_deferral_cannot_consume_resume_or_undo_clean() {
        for until in [None, Some(100.)] {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("db")).await.unwrap();
            let session = "TTEAM:CROOM:100.1".to_owned();
            let id = attention::intake(
                &store,
                Message {
                    event_id: "event".into(),
                    workspace: "TTEAM".into(),
                    channel: "CROOM".into(),
                    ts: "100.1".into(),
                    thread_ts: None,
                    sender: "UALICE".into(),
                    text: "<@UOWNER> help".into(),
                    files: vec![],
                    source: "socket".into(),
                    meta: None,
                    attachments: vec![],
                },
                "UOWNER".into(),
                20.,
                900.,
                "ask".into(),
            )
            .await
            .unwrap()
            .unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Pause {
                    reason: "Hold".into(),
                },
                Authority::Owner,
                20.,
            )
            .await
            .unwrap();
            attention::claim_due(&store, session.clone(), 20.)
                .await
                .unwrap()
                .unwrap();
            let paused = load(&store, id, session.clone()).await.unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Resume,
                Authority::Owner,
                21.,
            )
            .await
            .unwrap();
            assert_eq!(
                settle(&store, id, &paused, "observe: paused".into(), until)
                    .await
                    .unwrap(),
                Step::Stale
            );
            let state: (String, f64, String) = store
                .call(move |c| {
                    Ok(c.query_row(
                        "SELECT state,not_before,payload_json FROM thread_inbox WHERE id=?",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                    )?)
                })
                .await
                .unwrap();
            assert_eq!(state.0, "pending");
            assert_eq!(state.1, 0.);
            assert_eq!(
                serde_json::from_str::<Value>(&state.2).unwrap()["owner_trigger"],
                true
            );
            attention::claim_due(&store, session.clone(), 21.)
                .await
                .unwrap()
                .unwrap();
            let before_clean = load(&store, id, session.clone()).await.unwrap();
            controls::apply(
                &store,
                session.clone(),
                Control::Clean,
                Authority::Owner,
                22.,
            )
            .await
            .unwrap();
            controls::apply(&store, session, Control::Restore, Authority::Owner, 23.)
                .await
                .unwrap();
            assert_eq!(
                settle(
                    &store,
                    id,
                    &before_clean,
                    "observe: old input".into(),
                    until
                )
                .await
                .unwrap(),
                Step::Stale
            );
            let state: String = store
                .call(move |c| {
                    Ok(
                        c.query_row("SELECT state FROM thread_inbox WHERE id=?", [id], |r| {
                            r.get(0)
                        })?,
                    )
                })
                .await
                .unwrap();
            assert_eq!(state, "dropped");
        }
    }
}
