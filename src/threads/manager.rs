//! Bounded actor scheduling. The inbox is the work queue; notifications only
//! accelerate a sweep, so a lost notification cannot lose durable work.
use super::actor::{Actor, Step};
use crate::core::parent::Parent;
use anyhow::{bail, Result};
use std::sync::Arc;
use tokio::{sync::Mutex, task::JoinSet};

pub struct Manager<P: Parent> {
    actor: Arc<Actor<P>>,
    max_actors: usize,
    pass: Mutex<()>,
}
impl<P: Parent + 'static> Manager<P> {
    pub fn new(actor: Arc<Actor<P>>, max_actors: usize) -> Result<Self> {
        if max_actors == 0 || max_actors > 128 {
            bail!("actor concurrency must be between 1 and 128");
        }
        Ok(Self {
            actor,
            max_actors,
            pass: Mutex::new(()),
        })
    }
    pub async fn sweep(&self) -> Result<usize> {
        let _pass = self.pass.lock().await;
        let now = self.actor.clock.now();
        let limit = self.max_actors as i64;
        let sessions: Vec<String> = self
            .actor
            .store
            .call(move |c| {
                Ok(c.prepare(
                    "SELECT session_id FROM thread_inbox WHERE state='pending' AND not_before<=?
                GROUP BY session_id ORDER BY MIN(id) LIMIT ?",
                )?
                .query_map(rusqlite::params![now, limit], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?)
            })
            .await?;
        let mut tasks = JoinSet::new();
        for session in sessions {
            let actor = self.actor.clone();
            tasks.spawn(async move {
                let mut count = 0;
                // Busy threads yield to other threads after a bounded turn batch.
                for _ in 0..64 {
                    match actor.step(session.clone()).await? {
                        Step::Committed | Step::Observed => count += 1,
                        Step::Idle
                        | Step::Deferred
                        | Step::Failed
                        | Step::Stale
                        | Step::Unsupported => break,
                    }
                }
                Ok::<usize, anyhow::Error>(count)
            });
        }
        let mut count = 0;
        let mut failure = None;
        // Drain every task even if one fails; never abort unrelated thread work
        // merely because another actor encountered a storage error.
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(n)) => count += n,
                Ok(Err(error)) => failure = Some(error),
                Err(error) => failure = Some(error.into()),
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(count)
    }
}
