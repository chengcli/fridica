//! A finished discussion produces a separate, ordered channel post. Its durable
//! origin fences newer discussion work; it never closes mention obligations.
use super::actor::{self, Actor, Step};
use crate::{
    attention::{self, Capacity},
    core::{
        delivery::Post,
        parent::{Debrief, Decision, Discussion, Parent, ParentRequest, ReplyStatus},
    },
    store::outbox,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

pub(super) fn enqueue(
    c: &Connection,
    decision: &Decision,
    session: &str,
    inbox: i64,
    now: f64,
) -> Result<()> {
    if !decision.reply.as_ref().is_some_and(|r| {
        r.discussion == Discussion::Finished && matches!(r.status, ReplyStatus::Complete)
    }) {
        return Ok(());
    }
    let after = format!("{inbox}:reply");
    let origin: Option<(i64,i64,String)> = c.query_row("SELECT t.version,t.turns,o.trigger_class FROM threads t JOIN outbox o ON o.session_id=t.id WHERE t.id=? AND t.status='complete' AND o.idem_key=?",params![session,after],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((version, turn, class)) = origin {
        c.execute("INSERT INTO thread_inbox(session_id,kind,ref,payload_json,created) VALUES(?,'debrief',?,?,?)",params![session,inbox.to_string(),json!({"after":after,"version":version,"turn":turn,"class":class}).to_string(),now])?;
    }
    Ok(())
}

pub(super) async fn handle<P: Parent>(
    actor: &Actor<P>,
    mut request: ParentRequest,
) -> Result<Step> {
    let id = request.inbox_id;
    let session = request.session["id"]
        .as_str()
        .context("missing debrief session")?
        .to_owned();
    let origin = &request.trigger["payload"];
    let turn = origin["turn"].as_i64().unwrap_or(0);
    if turn <= request.session["debriefed_turn"].as_i64().unwrap_or(0)
        || origin["version"] != request.session["version"]
        || origin["turn"] != request.session["turns"]
        || request.session["status"] != "complete"
    {
        return actor::settle(
            &actor.store,
            id,
            &request,
            "observe: superseded debrief".into(),
            None,
        )
        .await;
    }
    let class = origin["class"].as_str().unwrap_or("human").to_owned();
    if let Capacity::Deferred(_) = attention::reserve(
        &actor.store,
        session.clone(),
        id,
        class.clone(),
        actor.clock.now(),
        actor.ids.next("reply"),
        actor.limits.clone(),
    )
    .await?
    {
        return Ok(Step::Deferred);
    }
    request.call = "debrief".into();
    let (raw, call) = actor.call(&request).await?;
    let text = raw
        .and_then(|v| serde_json::from_value::<Debrief>(v).ok())
        .map(|d| d.debrief.trim().to_owned())
        .filter(|s| !s.is_empty() && s.chars().count() <= 2500);
    let now = actor.clock.now();
    let owner = actor.owner.clone();
    actor.store.call(move|c| {
        let tx=c.transaction()?;
        let active:bool=tx.query_row("SELECT control='active' AND version=? AND debriefed_turn<? AND EXISTS(SELECT 1 FROM thread_inbox WHERE id=? AND state='processing') FROM threads WHERE id=?",params![request.session["version"].as_i64(),turn,id,session],|r|r.get(0))?;
        let notes_changed=super::effects::notes(&tx,&session)?.0 != request.session["notes"]["revision"].as_i64().unwrap_or(0);
        if !active || notes_changed {
            tx.execute("UPDATE thread_inbox SET state='pending' WHERE id=? AND state='processing'",[id])?;
            tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
            tx.commit()?;
            return Ok(Step::Stale);
        }
        if let Some(text)=&text {
            let post=Post {idem_key:format!("{id}:debrief"),session_id:session.clone(),kind:"debrief_root".into(),channel:request.session["channel"].as_str().context("missing channel")?.into(),thread_ts:None,text:format!("Debrief: this discussion is finished.\n\n{text}"),meta:Some(json!({"owner":owner,"session":session,"turn":turn,"status":"complete","kind":"debrief_root","worker":"","v":2})),filename:String::new(),blob:None,after:request.trigger["payload"]["after"].as_str().context("missing debrief prerequisite")?.into()};
            let post_id=outbox::enqueue_tx(&tx,&post,now)?;
            tx.execute("UPDATE outbox SET trigger_class=? WHERE id=?",params![class,post_id])?;
            tx.execute("UPDATE reply_reservations SET outbox_id=? WHERE inbox_id=? AND state='reserved'",params![post_id,id])?;
            tx.execute("UPDATE threads SET debriefed_turn=?,updated=?,version=version+1 WHERE id=?",params![turn,now,session])?;
        } else {
            tx.execute("UPDATE reply_reservations SET state='released' WHERE inbox_id=? AND outbox_id IS NULL",[id])?;
            tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,'system','debrief.unavailable',?,?)",params![now,session,json!({"inbox_id":id}).to_string()])?;
        }
        tx.execute("INSERT INTO parent_turns(session_id,inbox_id,backend,call,action_json,response_json,context_json,created) VALUES(?,?,'adapter','debrief',?,?,?,?)",params![session,id,json!({"debrief":text}).to_string(),call["response"].to_string(),serde_json::to_string(&request)?,now])?;
        tx.execute("UPDATE thread_inbox SET state='done' WHERE id=?",[id])?;
        tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('debrief_commit',?,?)",params![now,json!({"inbox_id":id,"request":request,"debrief":text}).to_string()])?;
        tx.commit()?;
        Ok(Step::Committed)
    }).await
}
