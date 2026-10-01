//! Drains the durable outbox through a delivery adapter, a bounded batch per
//! pass; the outcome mapping (sent, retry, ambiguous, rejected) is the store's.
use crate::{
    core::{
        delivery::{Delivery, DeliveryOutcome},
        time::Clock,
    },
    store::{outbox, Store},
};
use anyhow::Result;
use fridica_core::{
    delivery::ClaimedPost,
    egress::{self, DenyList},
};
use rusqlite::params;
use std::{path::Path, sync::Arc, time::Duration};

pub struct Dispatcher<D: Delivery> {
    pub store: Store,
    pub delivery: Arc<D>,
    pub clock: Arc<dyn Clock>,
    pub owner: String,
    pub observe_only: bool,
    pub timeout: Duration,
}
impl<D: Delivery> Dispatcher<D> {
    /// Bound each pass so intake/control tasks cannot be starved by a busy queue.
    pub async fn drain(&self, limit: usize) -> Result<usize> {
        self.drain_checked(limit, None).await
    }
    /// Like `drain`, with the owner's egress deny list (re-read every pass, as
    /// it may be edited at any time). A post that breaks a rule is not sent:
    /// it fails with `egress_<rule>`, which names the rule, never the term.
    /// An unreadable list holds every post back rather than sending unchecked.
    pub async fn drain_checked(&self, limit: usize, deny: Option<&Path>) -> Result<usize> {
        if self.observe_only {
            return Ok(0);
        }
        let rules = match deny.map(crate::config::egress::deny_list).transpose() {
            Ok(rules) => rules.unwrap_or_default(),
            Err(_) => {
                let now = self.clock.now();
                self.store.call(move |c| {
                    c.execute("INSERT INTO health_events(kind,details_json,created) SELECT 'egress_deny_list_unavailable','{}',? WHERE NOT EXISTS(SELECT 1 FROM health_events WHERE kind='egress_deny_list_unavailable' AND created>?)",params![now,now-3600.])?;
                    Ok(())
                }).await?;
                return Ok(0);
            }
        };
        let mut sent = 0;
        let mut attempted = 0;
        while attempted < limit {
            let batch =
                outbox::ready(&self.store, self.clock.now(), (limit - attempted).min(20)).await?;
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for id in batch {
                let Some(post) = outbox::claim_id(&self.store, self.clock.now(), id).await? else {
                    continue;
                };
                attempted += 1;
                let refused = check(&post, &rules);
                let result = if let Some(rule) = refused {
                    DeliveryOutcome::Rejected {
                        code: format!("egress_{}", rule.replace(':', "_")),
                    }
                } else {
                    tokio::time::timeout(self.timeout, self.delivery.send(post.clone()))
                        .await
                        .unwrap_or(DeliveryOutcome::Ambiguous {
                            code: "delivery_timeout".into(),
                        })
                };
                if outbox::complete(
                    &self.store,
                    post,
                    result,
                    self.owner.clone(),
                    self.clock.now(),
                )
                .await?
                {
                    sent += 1;
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        Ok(sent)
    }
}
/// The text a post would publish: its message, its file name and, for an
/// upload of text, its contents.
fn check(post: &ClaimedPost, rules: &DenyList) -> Option<String> {
    let p = &post.post;
    let upload = p.blob.as_deref().and_then(|b| std::str::from_utf8(b).ok());
    [Some(p.text.as_str()), Some(p.filename.as_str()), upload]
        .into_iter()
        .flatten()
        .find_map(|text| egress::scan(text, rules))
}
