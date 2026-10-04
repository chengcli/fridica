//! Explicit owner-requested historical mention review. Never called by migration
//! or startup, and never infer an answer from an unrelated historical post.
use crate::{config::Config, core::Authority, store::Store};
use anyhow::{bail, Result};
use fridica_core::store::{Backfill, HistoricalObligation, MentionQuery, Store as _};
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
    store.transact(move |u| {
        let identity=json!({"request":request,"workspace":config.slack.workspace,"channels":config.slack.channels,"owner":config.owner.slack_user});
        if request.apply {
            let prior=u.backfill_record(&request.client_id)?;
            if let Some(prior)=prior {
                let prior:Value=serde_json::from_str(&prior)?;
                if prior["identity"]!=identity {bail!("backfill client ID conflict");}
                return Ok(prior["result"].clone());
            }
        }
        let candidates=u.historical_mentions(&MentionQuery{workspace:config.slack.workspace.clone(),channels:json!(config.slack.channels).to_string(),since:request.since,until:request.until,owner:config.owner.slack_user.clone(),mention:format!("<@{}>",config.owner.slack_user)})?;
        if candidates.len()>1000 {bail!("backfill exceeds 1000 mentions; select a narrower time range");}
        let mut items=vec![];
        let mut obligations=vec![];
        for mention in candidates {
            let key=format!("mention:{}:{}:{}",mention.workspace,mention.channel,mention.ts);
            let id=format!("backfill:{key}");
            if request.apply {
                obligations.push(HistoricalObligation{id:id.clone(),session:mention.session.clone(),dedup_key:key,event_id:mention.event_id.clone(),
                    source:json!({"event_id":mention.event_id,"backfill":true,"answer_status":"unknown"}).to_string(),created:mention.received_at,due:now+config.attention.mention_grace,
                    state:json!({"kind":"deferred","reason":"Explicit historical backfill; prior answer status is unknown","until":now+config.attention.mention_grace}).to_string(),updated:now});
            }
            items.push(json!({"id":id,"session_id":mention.session,"event_id":mention.event_id,"timestamp":mention.ts}));
        }
        let result=json!({"applied":request.apply,"count":items.len(),"items":items,"answer_status":"unknown","due":if request.apply{Some(now+config.attention.mention_grace)}else{None}});
        if request.apply {
            u.apply_backfill(&Backfill{obligations,time:now,actor:config.owner.slack_user.clone(),client_id:request.client_id.clone(),result:result.to_string()})?;
            u.record("obligations_backfill",now,&json!({"request":request,"identity":identity,"result":result}).to_string(),true)?;
        }
        Ok(result)
    }).await
}
