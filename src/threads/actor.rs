//! A durable actor for message, owner-instruction and obligation-due turns.
//! Parent I/O never holds a database transaction. Version checks fence owner
//! controls arriving during a call; all resulting state effects commit together.
use crate::{
    attention::{self, Answer, Capacity},
    config::Attention,
    core::{
        parent::{Decision, Disposition, Parent, ParentRequest, ReplyStatus},
        policy::{attention_gate, GateInput},
        time::{Clock, Identifiers},
    },
    store::Store,
};
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::params;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
        let Some((id, kind)) =
            attention::claim_due(&self.store, session.clone(), self.clock.now()).await?
        else {
            return Ok(Step::Idle);
        };
        match self.handle(id, &kind, session.clone()).await {
            Ok(step) => Ok(step),
            Err(_error) => {
                let now = self.clock.now();
                self.store.call(move|c| {
                    let tx=c.transaction()?;
                    let attempts:i64=tx.query_row("SELECT attempts FROM thread_inbox WHERE id=?",[id],|r|r.get(0))?;
                    tx.execute("UPDATE thread_inbox SET attempts=attempts+1,state=?,not_before=? WHERE id=? AND state='processing'",params![if attempts>=2{"dropped"}else{"pending"},now+30.,id])?;
                    tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
                    tx.execute("INSERT OR IGNORE INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,updated)
                        SELECT ?,?,'signal',?,?,'An inbox turn failed; review required',?,?,?
                        WHERE NOT EXISTS(SELECT 1 FROM thread_inbox i JOIN obligations o ON i.ref=o.id WHERE i.id=? AND i.kind='obligation_due' AND o.kind='signal')",params![format!("inbox-failed:{id}"),session,format!("inbox-failed:{id}"),json!({"inbox_id":id}).to_string(),now,now,now,id])?;
                    tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'system','inbox.failed',?,?)",params![now,session,json!({"inbox_id":id,"attempt":attempts+1}).to_string()])?;
                    tx.commit()?;Ok(())
                }).await?;
                Ok(Step::Failed)
            }
        }
    }

    async fn handle(&self, id: i64, kind: &str, session: String) -> Result<Step> {
        let request = load(&self.store, id, session.clone()).await?;
        if self.observe_only || request.session["control"] != "active" {
            let reason = if self.observe_only {
                "observe-only mode"
            } else {
                "thread is paused or closed"
            };
            settle(
                &self.store,
                id,
                request.trigger.clone(),
                format!("observe: {reason}"),
            )
            .await?;
            return Ok(Step::Observed);
        }
        if !matches!(kind, "message" | "owner_instruction" | "obligation_due") {
            let now = self.clock.now();
            self.store
                .call(move |c| {
                    c.execute(
                        "UPDATE thread_inbox SET state='pending',not_before=? WHERE id=?",
                        params![now + 60., id],
                    )?;
                    Ok(())
                })
                .await?;
            return Ok(Step::Unsupported);
        }
        if kind == "obligation_due" {
            let ref_id = request.trigger["ref"].as_str().unwrap_or("");
            if !request.obligations.iter().any(|o| {
                o["id"] == ref_id
                    && matches!(o["state"].as_str(), Some("open" | "deferred"))
                    && o["due"].as_f64().is_some_and(|due| due <= self.clock.now())
            }) {
                settle(
                    &self.store,
                    id,
                    request.trigger.clone(),
                    "observe: obligation no longer due".into(),
                )
                .await?;
                return Ok(Step::Observed);
            }
        }
        let mut turn = request.session["turns"]
            .as_u64()
            .unwrap_or(0)
            .saturating_add(1);
        let mut verdict = "respond: owner instruction or attention due".to_owned();
        let trigger = if kind == "owner_instruction" {
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
                general_messages: false,
                cooling: false,
                observe_only: false,
                resumed: request.trigger["payload"]["resumed"] == true,
            };
            // Addressed messages in a blocked thread reach the parent so it can
            // decide whether the new instruction actually resolves the blocker.
            let outcome = attention_gate(&gate, gate.text.contains(&format!("<@{}>", self.owner)));
            turn = outcome.turn;
            verdict = format!("{}: {}", outcome.kind, outcome.reason);
            if matches!(outcome.kind.as_str(), "ignore" | "observe") {
                settle(&self.store, id, request.trigger.clone(), verdict).await?;
                return Ok(Step::Observed);
            }
            if outcome.kind == "triage" {
                // General-message triage is not ported yet. Retain this item for
                // the next runtime stage rather than silently changing its verdict.
                let now = self.clock.now();
                self.store
                    .call(move |c| {
                        c.execute(
                            "UPDATE thread_inbox SET state='pending',not_before=? WHERE id=?",
                            params![now + 60., id],
                        )?;
                        Ok(())
                    })
                    .await?;
                return Ok(Step::Unsupported);
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
        let mut request = request;
        let mut calls = Vec::new();
        let mut result = None;
        for round in 0..2 {
            request.call = if round == 0 { "decide" } else { "repair" }.into();
            let started = self.clock.now();
            let encoded = serde_json::to_string(&request)?;
            let call_id=self.store.call(move|c| {
                c.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('parent_call',?,?,0)",params![started,encoded])?;
                Ok(c.last_insert_rowid())
            }).await?;
            let response =
                tokio::time::timeout(self.parent_timeout, self.parent.decide(request.clone()))
                    .await;
            let (raw, error, failure) = match response {
                Ok(Ok(value)) => (Some(value), None, None),
                Ok(Err(failure)) => (None, Some("parent_unavailable"), Some(failure)),
                Err(_) => (None, Some("parent_timeout"), None),
            };
            let now = self.clock.now();
            let outcome = json!({"call_id":call_id,"response":raw,"error":error,"failure":failure});
            self.store.call(move|c| {
                let tx=c.transaction()?;
                tx.execute("UPDATE replay_events SET complete=1 WHERE seq=?",[call_id])?;
                tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('parent_result',?,?)",params![now,outcome.to_string()])?;
                tx.commit()?;Ok(())
            }).await?;
            let raw = raw.ok_or_else(|| anyhow!("parent call failed"))?;
            calls.push(json!({"request":request,"response":raw,"created":now}));
            match validate(&raw, &request, now) {
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
        commit(
            &self.store,
            id,
            session,
            request,
            decision,
            calls,
            verdict,
            turn,
            &self.owner,
            self.clock.now(),
            self.limits.streak_signal,
        )
        .await
    }
}

async fn load(store: &Store, id: i64, session: String) -> Result<ParentRequest> {
    store.call(move|c| {
        let tx=c.transaction()?;
        let data:String=tx.query_row("SELECT json_object('id',id,'workspace',workspace,'channel',channel,'root_ts',root_ts,'control',control,'status',status,'version',version,'turns',turns,'wait_streak',wait_streak,'no_progress',no_progress,'summary',summary,'last_reply_hash',last_reply_hash,'reset_at',reset_at,'context',json(context_json)) FROM threads WHERE id=?",[&session],|r|r.get(0))?;
        let (kind,reference,payload):(String,String,String)=tx.query_row("SELECT kind,ref,payload_json FROM thread_inbox WHERE id=? AND state='processing'",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let mut trigger=json!({"kind":kind,"ref":reference,"payload":serde_json::from_str::<Value>(&payload)?});
        let message_sql="SELECT json_object('event_id',event_id,'ts',ts,'sender',sender,'text',text,'meta',json(meta_json),'attachments',json(attachments_json)) FROM messages";
        if kind=="message" {
            let raw:String=tx.query_row(&format!("{message_sql} WHERE event_id=?"),[&reference],|r|r.get(0))?;
            trigger["message"]=serde_json::from_str(&raw)?;
        }
        if kind=="obligation_due" {
            let peer:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM obligations o JOIN messages m ON m.event_id=json_extract(o.source_json,'$.event_id') WHERE o.id=? AND m.meta_json IS NOT NULL)",[&reference],|r|r.get(0))?;
            trigger["source_peer"]=json!(peer);
        }
        let history:Vec<String>=tx.prepare(&format!("{message_sql} WHERE workspace||':'||channel||':'||root_ts=? ORDER BY CAST(ts AS REAL) DESC LIMIT 50"))?
            .query_map([&session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let obligations:Vec<String>=tx.prepare("SELECT json_object('id',id,'kind',kind,'summary',summary,'due',due,'state',state,'disposition',json(state_json),'source',json(source_json),'deliveries',json((SELECT COALESCE(json_group_array(json_object('id',p.outbox_id,'state',o.state,'error',o.error)), '[]') FROM obligation_posts p JOIN outbox o ON o.id=p.outbox_id WHERE p.obligation_id=obligations.id))) FROM obligations WHERE session_id=? AND state IN ('open','deferred','awaiting_delivery') ORDER BY created,id")?
            .query_map([&session],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let result=ParentRequest{inbox_id:id,call:"decide".into(),session:serde_json::from_str(&data)?,trigger,
            history:history.iter().rev().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,
            obligations:obligations.iter().map(|s|serde_json::from_str(s)).collect::<std::result::Result<_,_>>()?,previous:None,errors:vec![]};
        tx.commit()?;Ok(result)
    }).await
}

fn validate(raw: &Value, request: &ParentRequest, now: f64) -> Result<Decision> {
    let decision: Decision =
        serde_json::from_value(raw.clone()).context("invalid parent response schema")?;
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
        if reply.text.trim().is_empty() || reply.text.chars().count() > 7000 {
            bail!("reply must contain 1 to 7000 characters");
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

async fn settle(store: &Store, id: i64, trigger: Value, verdict: String) -> Result<()> {
    store.call(move|c| {
        let tx=c.transaction()?;
        if let Some(event)=trigger["message"]["event_id"].as_str() {tx.execute("UPDATE messages SET verdict=? WHERE event_id=?",params![verdict,event])?;}
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?",[id])?;
        tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
        tx.commit()?;Ok(())
    }).await
}

#[allow(clippy::too_many_arguments)]
async fn commit(
    store: &Store,
    id: i64,
    session: String,
    request: ParentRequest,
    decision: Decision,
    calls: Vec<Value>,
    verdict: String,
    turn: u64,
    owner: &str,
    now: f64,
    signal: usize,
) -> Result<Step> {
    let owner = owner.to_owned();
    let sql_turn = i64::try_from(turn).context("turn counter exceeds supported range")?;
    store.call(move|c| {
        let tx=c.transaction()?;
        let version:i64=tx.query_row("SELECT version FROM threads WHERE id=?",[&session],|r|r.get(0))?;
        if Some(version)!=request.session["version"].as_i64() {
            tx.execute("UPDATE thread_inbox SET state='pending' WHERE id=? AND state='processing'",[id])?;
            tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
            tx.commit()?;return Ok(Step::Stale);
        }
        let mut waiting=request.session["wait_streak"].as_i64().unwrap_or(0);
        let mut quiet=request.session["no_progress"].as_i64().unwrap_or(0)+1;
        let mut status=request.session["status"].as_str().unwrap_or("new").to_string();
        if decision.reopen_blocked && status=="blocked" {status="complete".into();}
        let mut hash=request.session["last_reply_hash"].as_str().unwrap_or("").to_owned();
        if let Some(reply)=&decision.reply {
            let candidate=format!("{:x}",Sha256::digest(reply.text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().as_bytes()));
            let explicit=request.trigger["kind"]=="owner_instruction" || request.trigger["message"]["text"].as_str().is_some_and(|s|s.contains(&format!("<@{owner}>")));
            let duplicate=candidate==hash && reply.status.as_str()==status && !explicit && reply.answers.is_empty();
            if !duplicate {
                let answer=Answer{key:format!("{id}:reply"),session:session.clone(),channel:request.session["channel"].as_str().context("missing channel")?.into(),
                    thread_ts:request.session["root_ts"].as_str().context("missing root timestamp")?.into(),text:reply.text.clone(),obligations:reply.answers.clone(),inbox:id};
                let post=attention::queue_answer_tx(&tx,&answer,now)?;
                let meta=json!({"owner":owner,"session":session,"turn":turn,"status":reply.status.as_str(),"kind":"reply","worker":"","v":2});
                tx.execute("UPDATE outbox SET meta_json=?,trigger_event=? WHERE id=?",params![meta.to_string(),request.trigger["message"]["event_id"].as_str().unwrap_or(""),post])?;
                quiet=if candidate==hash {quiet}else{0};hash=candidate;
                waiting=if matches!(reply.status,ReplyStatus::Waiting) {waiting+1}else{0};
                status=reply.status.as_str().into();
            }
        }
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
        if let Some(event)=request.trigger["message"]["event_id"].as_str() {tx.execute("UPDATE messages SET verdict=? WHERE event_id=?",params![verdict,event])?;}
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?",[id])?;
        tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
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
