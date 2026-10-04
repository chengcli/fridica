//! Fridica's Slack journal: every boundary that fridica-slack reports goes to
//! the private replay ledger, with Socket Mode status and identity in `meta`.
use crate::{core::time::Clock, store::Shared};
use fridica_core::store::{SlackIdentity, Store as _};
use fridica_slack::{BoxFuture, Identity, Recording, Status};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct StoreJournal {
    pub store: Shared,
    pub clock: Arc<dyn Clock>,
    /// Keep complete Slack bodies (`state.record = "full"`); otherwise they
    /// are summarized (#116).
    pub full: bool,
}
impl StoreJournal {
    fn time(&self) -> Result<f64, Recording> {
        let now = self.clock.now();
        now.is_finite().then_some(now).ok_or(Recording)
    }
}
impl fridica_slack::Journal for StoreJournal {
    fn now(&self) -> f64 {
        self.clock.now()
    }
    fn record(
        &self,
        kind: &'static str,
        payload: Value,
        complete: bool,
    ) -> BoxFuture<'_, Result<i64, Recording>> {
        let payload = if self.full {
            payload
        } else {
            crate::store::record::slack(kind, payload)
        };
        Box::pin(async move {
            let now = self.time()?;
            self.store
                .transact(move |u| u.record(kind, now, &payload.to_string(), complete))
                .await
                .map_err(|_| Recording)
        })
    }
    fn complete(
        &self,
        call: i64,
        kind: &'static str,
        payload: Value,
        complete: bool,
    ) -> BoxFuture<'_, Result<(), Recording>> {
        let payload = if self.full {
            payload
        } else {
            crate::store::record::slack(kind, payload)
        };
        Box::pin(async move {
            let now = self.time()?;
            self.store
                .transact(move |u| {
                    u.record(kind, now, &payload.to_string(), complete)?;
                    u.complete(call, complete)
                })
                .await
                .map_err(|_| Recording)
        })
    }
    fn identity(&self, identity: &Identity) -> BoxFuture<'_, Result<(), Recording>> {
        let scopes = identity
            .scopes
            .as_ref()
            .map(|s| s.iter().cloned().collect::<Vec<_>>().join(","))
            .unwrap_or("unknown".into());
        let names = json!(identity.channel_names).to_string();
        let team = identity.workspace_name.clone();
        Box::pin(async move {
            let identity = SlackIdentity {
                scopes,
                channels: names,
                workspace: team,
            };
            self.store
                .transact(move |u| u.keep_identity(&identity))
                .await
                .map_err(|_| Recording)
        })
    }
    fn socket_state(
        &self,
        status: Status,
        record: Value,
        complete: bool,
        failed: bool,
    ) -> BoxFuture<'_, Result<(), Recording>> {
        Box::pin(async move {
            let now = self.clock.now();
            let name = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or(Recording)?;
            self.store
                .transact(move |u| {
                    u.keep_socket_status(&name)?;
                    u.record("slack_socket_state", now, &record.to_string(), complete)?;
                    if failed {
                        u.note("slack_socket_failure", &record.to_string(), now)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|_| Recording)
        })
    }
    fn socket_recover(&self) -> BoxFuture<'_, Result<(), Recording>> {
        Box::pin(async move {
            let now = self.clock.now();
            self.store
                .transact(move |u| {
                    let previous = u.socket_status()?;
                    if previous
                        .as_deref()
                        .is_some_and(|s| matches!(s, "connecting" | "connected" | "reconnecting"))
                    {
                        u.note(
                            "slack_socket_interrupted",
                            &json!({"previous":previous}).to_string(),
                            now,
                        )?;
                    }
                    Ok(())
                })
                .await
                .map_err(|_| Recording)
        })
    }
}
