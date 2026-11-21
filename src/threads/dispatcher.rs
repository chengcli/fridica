//! Drains the durable outbox through a delivery adapter, a bounded batch per
//! pass; the outcome mapping (sent, retry, ambiguous, rejected) is the store's.
use super::verdict::{github_summary, key, verdicts, VERDICT};
use crate::{
    core::{
        delivery::{Delivery, DeliveryOutcome},
        time::Clock,
    },
    github::client::{Api, Operation, Request},
    store::Shared,
};
use anyhow::{bail, Result};
use fridica_core::{
    delivery::ClaimedPost,
    egress::{self, DenyList},
    store::{ArrivedMessage, PostOutcome, Store as _},
};
use std::{path::Path, sync::Arc, time::Duration};

pub struct Dispatcher<D: Delivery> {
    pub store: Shared,
    pub delivery: Arc<D>,
    pub clock: Arc<dyn Clock>,
    pub owner: String,
    pub observe_only: bool,
    pub timeout: Duration,
    /// Reads a PR's live head for verdict lines; None signs nothing.
    pub heads: Option<Arc<dyn Api>>,
}
impl<D: Delivery> Dispatcher<D> {
    /// Turns a `VERDICT <owner>/<repo>#<PR> <sha> <verdict>` line into one
    /// line on top (provision04): the SIGN-OFF line only if no other line has
    /// the word verdict, a person's ask or the job's brief names the PR
    /// (`review`), and its head, read now, is still `<sha>`; else why not.
    async fn sign(&self, text: &str, review: &[String]) -> String {
        let (lines, _) = verdicts(text);
        let Some(m) = lines.iter().find_map(|l| VERDICT.captures(l.trim_end())) else {
            return text.into();
        };
        let ((repo, n), sha, verdict) = (key(&m), &m[3], &m[4]);
        let live = match &self.heads {
            _ if lines.len() > 1 => Err("the word verdict appears on more than one line".into()),
            _ if review.is_empty() => Err("no review subject".into()),
            _ if !review.contains(&format!("{repo}#{n}")) => {
                Err(format!("the job reviews {}", review.join(", ")))
            }
            Some(api) => api
                .get(Request {
                    repo: repo.clone(),
                    operation: Operation::Pull { number: n },
                })
                .await
                .map_err(|e| format!("head not read: {}", e.message()))
                .and_then(|v| {
                    v["head"]["sha"]
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("head not read".into())
                }),
            None => Err("head not read: GitHub is off".into()),
        };
        let top = match live {
            Ok(live) if live == sha => format!("SIGN-OFF #{n} {sha} {verdict}"),
            Ok(live) => format!("#{n} not signed: its head moved from {sha} to {live}."),
            Err(why) => format!("#{n} not signed: {why}."),
        };
        let rest = text.lines().filter(|l| l.trim_end() != &m[0]);
        std::iter::once(top.as_str())
            .chain(rest)
            .collect::<Vec<_>>()
            .join("\n")
    }
    /// Bound each pass so intake/control tasks cannot be starved by a busy queue.
    pub async fn drain(&self, limit: usize) -> Result<usize> {
        self.drain_checked(limit, None).await
    }
    /// Like `drain`, with the owner's egress deny list (re-read every pass, as
    /// it may be edited at any time). A post that breaks a rule is not sent:
    /// it fails with `egress_<rule>`, which names the rule, never the term.
    /// An unreadable list holds every post back rather than sending unchecked.
    ///
    /// A SIGN-OFF line written as such is not checked here (#109); a verdict
    /// line is signed against the live head, in the copy that is sent.
    pub async fn drain_checked(&self, limit: usize, deny: Option<&Path>) -> Result<usize> {
        if self.observe_only {
            return Ok(0);
        }
        let rules = match deny.map(crate::config::egress::deny_list).transpose() {
            Ok(rules) => rules.unwrap_or_default(),
            Err(_) => {
                let now = self.clock.now();
                self.store
                    .transact(move |u| {
                        u.note_unless_since("egress_deny_list_unavailable", "{}", now, now - 3600.)
                    })
                    .await?;
                return Ok(0);
            }
        };
        let mut sent = 0;
        let mut attempted = 0;
        while attempted < limit {
            let (now, batch) = (self.clock.now(), (limit - attempted).min(20));
            let batch = self
                .store
                .transact(move |u| u.ready_posts(now, batch))
                .await?;
            if batch.is_empty() {
                break;
            }
            let mut progressed = false;
            for id in batch {
                let now = self.clock.now();
                let Some(post) = self
                    .store
                    .transact(move |u| u.claim_post(now, Some(id)))
                    .await?
                else {
                    continue;
                };
                attempted += 1;
                // Old durable posts must not revive the retired channel debrief workflow.
                if post.post.kind == "debrief_root" {
                    let owner = self.owner.clone();
                    self.store
                        .transact(move |u| {
                            u.finish_delivery(
                                &post,
                                &DeliveryOutcome::Rejected {
                                    code: "debrief_removed".into(),
                                },
                                &owner,
                                now,
                            )
                        })
                        .await?;
                    progressed = true;
                    continue;
                }
                let mut sending = post.clone();
                let meta = post.post.meta.as_ref().map(|m| m["review"].clone());
                let review: Vec<String> =
                    serde_json::from_value(meta.unwrap_or_default()).unwrap_or_default();
                sending.post.text = if post
                    .post
                    .meta
                    .as_ref()
                    .is_some_and(|m| m["github_reviews"] == true)
                {
                    github_summary(&post.post.text)
                } else {
                    // Compatibility for already queued legacy review reports.
                    self.sign(&post.post.text, &review).await
                };
                let text = sending.post.text.clone();
                // Every broken rule is recorded, each as `egress_<rule>`, so
                // the owner sees all of them in one pass.
                let refused = check(&sending, &rules);
                let result = if !refused.is_empty() {
                    DeliveryOutcome::Rejected {
                        code: refused
                            .iter()
                            .map(|rule| format!("egress_{}", rule.replace(':', "_")))
                            .collect::<Vec<_>>()
                            .join("+"),
                    }
                } else {
                    tokio::time::timeout(self.timeout, self.delivery.send(sending))
                        .await
                        .unwrap_or(DeliveryOutcome::Ambiguous {
                            code: "delivery_timeout".into(),
                        })
                };
                let (owner, now) = (self.owner.clone(), self.clock.now());
                // A stale attempt's late result commits before it is reported.
                let outcome = self
                    .store
                    .transact(move |u| {
                        let p = &post.post;
                        // The thread history keeps what was sent: the sign-off
                        // or why it was not signed, not the verdict line.
                        if let DeliveryOutcome::Sent { reference } = &result {
                            if text != p.text && !reference.is_empty() {
                                u.keep_message(&ArrivedMessage {
                                    event_id: format!("self:{}:{reference}", p.channel),
                                    workspace: p.session_id.split(':').next().unwrap_or("").into(),
                                    channel: p.channel.clone(),
                                    ts: reference.clone(),
                                    root_ts: p.thread_ts.clone().unwrap_or(reference.clone()),
                                    thread_ts: p.thread_ts.clone(),
                                    sender: owner.clone(),
                                    text,
                                    files: "[]".into(),
                                    source: "self".into(),
                                    meta: p.meta.as_ref().map(ToString::to_string),
                                    received_at: now,
                                    attachments: "[]".into(),
                                    mentions_owner: false,
                                })?;
                            }
                        }
                        let outcome = u.finish_delivery(&post, &result, &owner, now)?;
                        if refused.is_empty()
                            && p.kind == "upload"
                            && outcome == PostOutcome::Unsent
                        {
                            upload_notice(u, &post, now)?;
                        }
                        Ok(outcome)
                    })
                    .await?;
                if outcome == PostOutcome::Stale {
                    bail!("stale delivery attempt or changed payload");
                }
                if outcome == PostOutcome::Sent {
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
/// Read the committed disposition: a rate limit can leave the upload either
/// pending or permanently failed. Never resend bytes on an uncertain result.
fn upload_notice(
    u: &mut dyn fridica_core::store::Unit,
    claim: &ClaimedPost,
    now: f64,
) -> Result<()> {
    let Some(post) = u
        .thread_posts(&claim.post.session_id)?
        .into_iter()
        .find(|p| p.id == claim.id)
    else {
        return Ok(());
    };
    let text = match post.state.as_str() {
        "ambiguous" => format!("Slack did not confirm the upload of {} ({}). Check the thread before requesting another copy.",post.filename,post.error),
        "failed" => format!("The upload of {} failed ({}).",post.filename,post.error),
        _ => return Ok(()),
    };
    u.queue_post(
        &crate::core::delivery::Post {
            text,
            ..super::posts::post(
                format!("upload-notice:{}:{}", claim.id, claim.attempt),
                &post.session_id,
                "notice",
                &post.channel,
                post.thread_ts,
            )
        },
        now,
    )?;
    Ok(())
}

/// The text a post would publish: its message, its file name and, for an
/// upload of text, its contents.
fn check(post: &ClaimedPost, rules: &DenyList) -> Vec<String> {
    let p = &post.post;
    let upload = p.blob.as_deref().and_then(|b| std::str::from_utf8(b).ok());
    let mut broken: Vec<String> = [Some(p.text.as_str()), Some(p.filename.as_str()), upload]
        .into_iter()
        .flatten()
        .flat_map(|text| egress::scan(text, rules))
        .collect();
    broken.sort();
    broken.dedup();
    broken
}
