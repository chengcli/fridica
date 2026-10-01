//! Durable Socket Mode intake. Call only from an authenticated Slack connection;
//! this does not authenticate arbitrary HTTP requests or interpret owner controls.
use super::ingress::normalize;
use crate::{
    attention,
    config::Config,
    core::time::{Clock, Identifiers},
    store::Store,
};
use anyhow::{bail, Result};
pub use fridica_slack::Acknowledgement;
use fridica_slack::{socket::Refused, BoxFuture, Intake};
use rusqlite::params;
use serde_json::json;
use std::sync::Arc;

#[derive(Clone)]
pub struct Receiver {
    pub(crate) store: Store,
    pub(crate) config: Arc<Config>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) ids: Arc<dyn Identifiers>,
}
impl Receiver {
    pub fn new(
        store: Store,
        config: Arc<Config>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn Identifiers>,
    ) -> Self {
        Self {
            store,
            config,
            clock,
            ids,
        }
    }
    /// An acknowledgement becomes available only after the transaction commits.
    /// Transport loss after commit is safe: a repeated envelope is deduplicated
    /// by event ID and workspace/channel/timestamp, independently of envelope ID.
    pub async fn receive(&self, bytes: &[u8]) -> Result<Option<Acknowledgement>> {
        let fridica_slack::ingress::Envelope {
            kind,
            id,
            value: envelope,
        } = fridica_slack::ingress::envelope(bytes)?;
        let kind = kind.as_str();
        if matches!(kind, "hello" | "disconnect") {
            return Ok(None);
        }
        let payload = &envelope["payload"];
        let scoped = self.scoped(payload);
        let message = (kind == "events_api" && scoped)
            .then(|| normalize(payload, "socket"))
            .flatten();
        let dropped = kind == "events_api"
            && scoped
            && message.is_none()
            && payload["event"]["type"] == "message"
            && payload["event"]["text"]
                .as_str()
                .is_some_and(|s| s.contains(&format!("<@{}>", self.config.owner.slack_user)));
        let now = self.clock.now();
        if !now.is_finite() {
            bail!("invalid receive time");
        }
        let config = self.config.clone();
        let obligation = self.ids.next("obligation");
        // Only supported in-scope event callbacks retain full content. Other
        // envelope types can contain verification tokens and are never controls.
        let record = if kind == "events_api" && scoped {
            json!({"envelope":envelope,"normalized":message,"dropped_mention":dropped})
        } else {
            json!({"envelope_id":id,"type":kind,"ignored":true})
        };
        self.store.call(move |c| {
            let tx = c.transaction()?;
            tx.execute("INSERT INTO replay_events(kind,time,payload_json) VALUES('slack_envelope',?,?)",params![now,record.to_string()])?;
            if let Some(message) = message {
                attention::intake_tx(&tx,message,&config.owner.slack_user,now,config.attention.mention_grace,&obligation,None)?;
            }
            if dropped {
                tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('slack_dropped_mention',?,?)",
                    params![json!({"event_id":record["envelope"]["payload"]["event_id"],"reason":"unsupported_message"}).to_string(),now])?;
            }
            tx.commit()?;
            Ok(())
        }).await?;
        Ok(Some(Acknowledgement { envelope_id: id }))
    }
    pub(crate) fn scoped(&self, payload: &serde_json::Value) -> bool {
        payload["team_id"] == self.config.slack.workspace
            && payload["event"]["channel"]
                .as_str()
                .is_some_and(|c| self.config.slack.channels.iter().any(|v| v == c))
    }
}
impl Intake for Receiver {
    fn receive<'a>(
        &'a self,
        envelope: &'a [u8],
    ) -> BoxFuture<'a, std::result::Result<Option<Acknowledgement>, Refused>> {
        Box::pin(async move { Receiver::receive(self, envelope).await.map_err(|_| Refused) })
    }
}
