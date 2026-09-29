//! Explicit owner-requested historical mention review. Never called by migration
//! or startup, and never infer an answer from an unrelated historical post.
use crate::{config::Config, core::Authority, store::Store};
use anyhow::{bail, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub since: f64,
    pub until: f64,
    #[serde(default)]
    pub apply: bool,
    #[serde(default)]
    pub client_id: String,
}
impl Request {
    pub fn valid(&self, now: f64) -> bool {
        now.is_finite()
            && self.since.is_finite()
            && self.until.is_finite()
            && self.since >= 0.
            && self.until > self.since
            && self.until <= now
            && (!self.apply
                || (8..=80).contains(&self.client_id.len())
                    && self
                        .client_id
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-'))
    }
}
pub async fn run(
    store: &Store,
    config: &Config,
    request: Request,
    authority: Authority,
    now: f64,
) -> Result<Value> {
    if authority != Authority::Owner {
        bail!("backfill requires owner authentication");
    }
    if !request.valid(now) || !(now + config.attention.mention_grace).is_finite() {
        bail!("invalid historical backfill request");
    }
    let config = config.clone();
    store.call(move |c| {
        let tx=c.transaction()?;
        let identity=json!({"request":request,"workspace":config.slack.workspace,"channels":config.slack.channels,"owner":config.owner.slack_user});
        if request.apply {
            let prior:Option<String>=tx.query_row("SELECT payload_json FROM replay_events WHERE kind='obligations_backfill' AND json_extract(payload_json,'$.request.client_id')=?",[&request.client_id],|r|r.get(0)).optional()?;
            if let Some(prior)=prior {
                let prior:Value=serde_json::from_str(&prior)?;
                if prior["identity"]!=identity {bail!("backfill client ID conflict");}
                return Ok(prior["result"].clone());
            }
        }
        let mut candidates=vec![];
        let mut query=tx.prepare("SELECT m.event_id,t.id,m.workspace,m.channel,m.ts,m.received_at FROM messages m JOIN threads t ON t.workspace=m.workspace AND t.channel=m.channel AND t.root_ts=m.root_ts
            WHERE m.workspace=? AND m.channel IN (SELECT value FROM json_each(?))
            AND CAST(m.ts AS REAL)>=? AND CAST(m.ts AS REAL)<? AND m.sender!=? AND m.source!='self'
            AND instr(m.text,?)>0 AND CAST(m.ts AS REAL)>t.reset_at AND t.control NOT IN ('closed','archived','cleaned')
            AND NOT EXISTS(SELECT 1 FROM obligations o WHERE o.dedup_key='mention:'||m.workspace||':'||m.channel||':'||m.ts)
            ORDER BY CAST(m.ts AS REAL),m.event_id LIMIT 1001")?;
        let selected=query.query_map(params![config.slack.workspace,json!(config.slack.channels).to_string(),request.since,request.until,config.owner.slack_user,format!("<@{}>",config.owner.slack_user)],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,f64>(5)?)))?;
        for row in selected {candidates.push(row?);}
        drop(query);
        if candidates.len()>1000 {bail!("backfill exceeds 1000 mentions; select a narrower time range");}
        let mut items=vec![];
        for (event,session,workspace,channel,ts,created) in candidates {
            let key=format!("mention:{workspace}:{channel}:{ts}");
            let id=format!("backfill:{key}");
            if request.apply {
                tx.execute("INSERT INTO obligations(id,session_id,kind,dedup_key,source_json,summary,created,due,state,state_json,updated) VALUES(?,?,'mention',?,?,'Historical mention; answer status requires owner review',?,?,'deferred',?,?)",
                    params![id,session,key,json!({"event_id":event,"backfill":true,"answer_status":"unknown"}).to_string(),created,now+config.attention.mention_grace,json!({"kind":"deferred","reason":"Explicit historical backfill; prior answer status is unknown","until":now+config.attention.mention_grace}).to_string(),now])?;
                tx.execute("UPDATE messages SET mentions_owner=1 WHERE event_id=?",[&event])?;
            }
            items.push(json!({"id":id,"session_id":session,"event_id":event,"timestamp":ts}));
        }
        let result=json!({"applied":request.apply,"count":items.len(),"items":items,"answer_status":"unknown","due":if request.apply{Some(now+config.attention.mention_grace)}else{None}});
        if request.apply {
            tx.execute("INSERT INTO audit(time,actor,action,target,details_json) VALUES(?,?,'obligations.backfill',?,?)",params![now,config.owner.slack_user,request.client_id,result.to_string()])?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('obligations_backfill',?,?)",params![now,json!({"request":request,"identity":identity,"result":result}).to_string()])?;
        }
        tx.commit()?;
        Ok(result)
    }).await
}
