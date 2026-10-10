//! A finished discussion produces a separate, ordered channel post. Its durable
//! origin fences newer discussion work; it never closes mention obligations.
use super::actor::{self, Actor, Step};
use crate::{
    attention::{self, Capacity},
    core::{
        delivery::Post,
        parent::{Debrief, Decision, Discussion, Parent, ParentRequest, ReplyStatus},
    },
};
use anyhow::{Context, Result};
use fridica_core::store::{DebriefOrigin, DebriefPost, DebriefTurn, Store as _, Unit};
use serde_json::json;

pub(super) fn enqueue(
    u: &mut dyn Unit,
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
    if let Some(DebriefOrigin {
        version,
        turn,
        class,
    }) = u.debrief_origin(session, &after)?
    {
        let payload =
            json!({"after":after,"version":version,"turn":turn,"class":class}).to_string();
        u.queue_debrief(session, inbox, &payload, now)?;
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
    // A usage limit (#107) or a lost login refresh is temporary: the
    // debrief waits and is retried.
    if let Some((_, delay)) = super::actor::temporary(std::slice::from_ref(&call)) {
        let retry = actor.clock.now() + delay;
        return actor::settle(&actor.store, id, &request, String::new(), Some(retry)).await;
    }
    let text = raw
        .and_then(|v| serde_json::from_value::<Debrief>(v).ok())
        .map(|d| d.debrief.trim().to_owned())
        .filter(|s| !s.is_empty() && s.chars().count() <= 2500);
    let now = actor.clock.now();
    let owner = actor.owner.clone();
    actor.store.transact(move|u| {
        let active=u.debrief_due(&session,request.session["version"].as_i64(),turn,id)?;
        let notes_changed=super::effects::notes(u,&session)?.0 != request.session["notes"]["revision"].as_i64().unwrap_or(0);
        if !active || notes_changed {
            u.debrief_stale(id)?;
            return Ok(Step::Stale);
        }
        if let Some(text)=&text {
            let post=Post {idem_key:format!("{id}:debrief"),session_id:session.clone(),kind:"debrief_root".into(),channel:request.session["channel"].as_str().context("missing channel")?.into(),thread_ts:None,text:format!("Debrief: this discussion is finished.\n\n{text}"),meta:Some(json!({"owner":owner,"session":session,"turn":turn,"status":"complete","kind":"debrief_root","worker":"","v":2})),filename:String::new(),blob:None,after:request.trigger["payload"]["after"].as_str().context("missing debrief prerequisite")?.into()};
            let post_id=u.queue_post(&post,now)?;
            u.debrief_posted(&DebriefPost{session:session.clone(),inbox:id,post:post_id,class,turn,now})?;
        } else {
            u.debrief_unavailable(&session,id,now)?;
        }
        u.keep_debrief_turn(&DebriefTurn{session:session.clone(),inbox:id,action:json!({"debrief":text}).to_string(),response:call["response"].to_string(),context:serde_json::to_string(&request)?,created:now})?;
        u.inbox_done(id)?;
        u.record("debrief_commit",now,&json!({"inbox_id":id,"request":request,"debrief":text}).to_string(),true)?;
        Ok(Step::Committed)
    }).await
}
