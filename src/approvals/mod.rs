//! Owner approval service. Authentication belongs to the calling control adapter;
//! its authority is explicit and never taken from model/request JSON.
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        time::{Clock, Identifiers},
        worker::*,
        Authority,
    },
    store::Store,
    workers::protocol::ApprovalHandler,
};
use anyhow::{bail, Result};
pub use fridica_core::approvals as rules;
use fridica_core::store::{ApprovalSettlement, ApprovalStart, NewApproval, Store as _};
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, Mutex, Notify, Semaphore};

#[derive(Clone)]
pub struct Broker {
    store: Store,
    config: Arc<Mutex<Arc<Config>>>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn Identifiers>,
    changed: Arc<Notify>,
    capacity: Arc<Semaphore>,
    /// Optional UI wakeup containing only the durable ID. Dropped wakeups do
    /// not lose requests: consumers list pending approvals from the database.
    notifications: Option<mpsc::Sender<String>>,
}
impl Broker {
    pub fn new(
        store: Store,
        config: Arc<Config>,
        clock: Arc<dyn Clock>,
        ids: Arc<dyn Identifiers>,
        notifications: Option<mpsc::Sender<String>>,
    ) -> Self {
        Self {
            store,
            config: Arc::new(Mutex::new(config)),
            clock,
            ids,
            changed: Arc::new(Notify::new()),
            capacity: Arc::new(Semaphore::new(128)),
            notifications,
        }
    }
    pub async fn decide(
        &self,
        id: String,
        decision: ApprovalDecision,
        authority: Authority,
    ) -> Result<bool> {
        if authority != Authority::Owner {
            bail!("approval decisions require owner authentication");
        }
        let config = self.config.lock().await;
        let changed = self
            .settle(
                id,
                ApprovalSettlement::Decide(decision),
                config.owner.slack_user.clone(),
            )
            .await?;
        self.changed.notify_waiters();
        Ok(changed)
    }
    /// Settle request `id` as `actor`, now.
    async fn settle(
        &self,
        id: String,
        settlement: ApprovalSettlement,
        actor: String,
    ) -> Result<bool> {
        let now = self.clock.now();
        self.store
            .transact(move |u| u.settle_approval(&id, settlement, &actor, now))
            .await
    }
    async fn run(
        &self,
        worker: WorkerRecord,
        job: Job,
        request: ApprovalRequest,
        mut reply: oneshot::Sender<ApprovalDecision>,
    ) -> Result<()> {
        let config = self.config.lock().await;
        let Some(workspace) = config
            .machines
            .get(&worker.machine)
            .and_then(|m| m.workspace(&worker.workspace))
        else {
            return Ok(());
        };
        let policy = &workspace.policy;
        let Ok(timeout) = Duration::try_from_secs_f64(policy.approval_timeout) else {
            return Ok(());
        };
        let Some(deadline) = tokio::time::Instant::now().checked_add(timeout) else {
            return Ok(());
        };
        let now = self.clock.now();
        let id = self.ids.next("approval");
        let automatic = if policy.approvals == "never"
            || (worker.backend == "claude" && policy.claude_prompts == "none")
        {
            Some(ApprovalDecision::Deny)
        } else {
            rules::decide(policy, &request)
        };
        let approval = NewApproval {
            id: id.clone(),
            worker,
            job,
            request,
            automatic,
            now,
            expires_at: now + policy.approval_timeout,
        };
        let started = self
            .store
            .transact(move |u| u.begin_approval(&approval))
            .await?;
        drop(config);
        if let ApprovalStart::Immediate(decision) = started {
            let _ = reply.send(decision);
            return Ok(());
        }
        if let Some(notify) = &self.notifications {
            let _ = notify.try_send(id.clone());
        }
        loop {
            if reply.is_closed() {
                self.settle(id, ApprovalSettlement::Cancel, "interrupted".into())
                    .await?;
                return Ok(());
            }
            let lookup = id.clone();
            let Some(a) = self.store.transact(move |u| u.approval(&lookup)).await? else {
                return Ok(());
            };
            if a.status != "pending" {
                let decision = if a.status == "approved" {
                    if a.scope == "session" {
                        ApprovalDecision::Session
                    } else {
                        ApprovalDecision::Once
                    }
                } else {
                    ApprovalDecision::Deny
                };
                let _ = reply.send(decision);
                return Ok(());
            }
            if self.clock.now() >= a.expires_at || tokio::time::Instant::now() >= deadline {
                self.settle(id.clone(), ApprovalSettlement::Expire, "timeout".into())
                    .await?;
                // A control may already have committed a decision before the
                // timer fired. Read that committed result on the next pass.
                continue;
            }
            // Poll observes durable stop/recovery transactions and clock advances.
            tokio::select! {
                biased;
                _=reply.closed()=>{},
                _=tokio::time::sleep_until(deadline)=>{},
                _=self.changed.notified()=>{},
                _=tokio::time::sleep(Duration::from_millis(50))=>{},
            }
        }
    }
}
impl ApprovalHandler for Broker {
    fn reconfigure(&self, config: Arc<Config>) -> AdapterFuture<'_, Result<()>> {
        Box::pin(async move {
            let mut current = self.config.lock().await;
            let now = self.clock.now();
            self.store
                .transact(move |u| u.cancel_pending_approvals(now))
                .await?;
            *current = config;
            self.changed.notify_waiters();
            Ok(())
        })
    }

    fn request(
        &self,
        worker: WorkerRecord,
        job: Job,
        request: ApprovalRequest,
    ) -> AdapterFuture<'_, ApprovalDecision> {
        Box::pin(async move {
            let Ok(permit) = self.capacity.clone().try_acquire_owned() else {
                return ApprovalDecision::Deny;
            };
            let (reply, wait) = oneshot::channel();
            let broker = self.clone();
            // The owner task survives cancellation while a SQLite write is in
            // flight, then durably settles the request when its caller drops.
            tokio::spawn(async move {
                let _permit = permit;
                let _ = broker.run(worker, job, request, reply).await;
            });
            wait.await.unwrap_or(ApprovalDecision::Deny)
        })
    }
    fn cancel(&self, worker_id: String) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            let now = self.clock.now();
            let _ = self
                .store
                .transact(move |u| u.cancel_worker_approvals(&worker_id, now))
                .await;
            self.changed.notify_waiters();
        })
    }
}
