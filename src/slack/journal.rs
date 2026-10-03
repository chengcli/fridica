//! Fridica's Slack journal: every boundary that fridica-slack reports goes to
//! the private replay ledger, with Socket Mode status and identity in `meta`.
use crate::{
    core::time::Clock,
    store::{Sqlite, Store},
};
use fridica_core::store::{Health, Ledger, Store as _};
use fridica_slack::{BoxFuture, Identity, Recording, Status};
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Clone)]
pub struct StoreJournal {
    pub store: Store,
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
            self.store.call(move|c| {
                c.execute("INSERT INTO meta VALUES('slack_scopes',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[scopes])?;
                c.execute("INSERT INTO meta VALUES('slack_channel_names',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[names])?;
                c.execute("INSERT INTO meta VALUES('slack_workspace_name',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[team])?;
                Ok(())
            }).await.map_err(|_| Recording)
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
            self.store.call(move|c| {
                let tx=c.transaction()?;
                tx.execute("INSERT INTO meta VALUES('slack_status',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[&name])?;
                tx.execute("UPDATE runtime SET slack_status=? WHERE id=1",[name])?;
                Sqlite(&tx).record("slack_socket_state",now,&record.to_string(),complete)?;
                if failed {Sqlite(&tx).note("slack_socket_failure",&record.to_string(),now)?;}
                tx.commit()?;Ok(())
            }).await.map_err(|_| Recording)
        })
    }
    fn socket_recover(&self) -> BoxFuture<'_, Result<(), Recording>> {
        Box::pin(async move {
            let now = self.clock.now();
            self.store
                .call(move |c| {
                    let tx = c.transaction()?;
                    let previous: Option<String> = tx.query_row(
                        "SELECT (SELECT value FROM meta WHERE key='slack_status')",
                        [],
                        |r| r.get(0),
                    )?;
                    if previous
                        .as_deref()
                        .is_some_and(|s| matches!(s, "connecting" | "connected" | "reconnecting"))
                    {
                        Sqlite(&tx).note(
                            "slack_socket_interrupted",
                            &json!({"previous":previous}).to_string(),
                            now,
                        )?;
                    }
                    tx.commit()?;
                    Ok(())
                })
                .await
                .map_err(|_| Recording)
        })
    }
}
