//! Drains the durable outbox through a delivery adapter, a bounded batch per
//! pass; the outcome mapping (sent, retry, ambiguous, rejected) is the store's.
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
use regex::Regex;
use std::{
    path::Path,
    sync::{Arc, LazyLock},
    time::Duration,
};
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

static VERDICT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^VERDICT ([\w.-]+/[\w.-]+)#([0-9]{1,9}) ([0-9a-f]{40}) (approve \(code review\)|approve|changes)$",
    )
    .unwrap()
});
static SUBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"([\w.-]+/[\w.-]+)(?:#|/pull/| (?i:pr|pull) #)([0-9]{1,9})").unwrap()
});
/// The PR a verdict line or a brief names: (lowercased owner/repo, number).
fn key(m: &regex::Captures) -> (String, u64) {
    (m[1].to_lowercase(), m[2].parse().unwrap_or(0))
}

/// The PRs a job brief names (`<owner>/<repo>#<PR>` or a pull URL): the only
/// ones its verdict lines may sign.
pub(crate) fn subjects(brief: &str) -> Vec<String> {
    let names = SUBJECT.captures_iter(brief);
    names
        .map(|m| format!("{}#{}", key(&m).0, key(&m).1))
        .collect()
}
/// Whether a line holds the word verdict in any case or width (NFKC), with
/// marks (also those NFD splits off a letter) and Unicode's
/// Default_Ignorable_Code_Point characters left out (DerivedCoreProperties-
/// 18.0.0.txt: zero-width characters, the BOM, the soft hyphen, Hangul
/// fillers, variation selectors, tags).
fn says_verdict(line: &str) -> bool {
    let kept = line.nfkc().nfd().filter(|c| {
        !is_combining_mark(*c) && !matches!(c, '\u{AD}' | '\u{34F}' | '\u{61C}' | '\u{115F}'..='\u{1160}' | '\u{17B4}'..='\u{17B5}'
            | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
            | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}' | '\u{FFF0}'..='\u{FFF8}'
            | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' | '\u{E0000}'..='\u{E0FFF}')
            && (c.is_ascii() || c.is_alphanumeric() || c.is_whitespace())
    });
    kept.collect::<String>().to_lowercase().contains("verdict")
}
/// A text's lines holding the word verdict, inside code or not, and the rest.
fn verdicts(text: &str) -> (Vec<&str>, Vec<&str>) {
    text.lines().partition(|l| says_verdict(l))
}
const UNPARSED: &str = "Not signed: unparsed verdict line: ";
const PLACED: &str = "Not signed: the verdict line must be the first or last line: ";
/// The text with its verdict lines first, so a cut or split keeps them all in
/// the post; if one has "VERDICT", a line that does not parse as written, or
/// is not the text's first or last non-empty line, becomes its refusal now.
pub(crate) fn hoist(text: &str) -> String {
    let (lines, rest) = verdicts(text);
    if !lines.iter().any(|l| l.contains("VERDICT")) {
        return text.into();
    }
    let mut full = text.lines().filter(|l| !l.trim().is_empty());
    let ends = [full.next(), full.next_back()];
    let lines = lines.iter().map(|l| match VERDICT.is_match(l.trim_end()) {
        true if ends.contains(&Some(l)) => l.to_string(),
        true => format!("{PLACED}{}", l.trim()),
        false if l.starts_with(UNPARSED) || l.starts_with(PLACED) => l.to_string(),
        false => format!("{UNPARSED}{}", l.trim()),
    });
    let rest = rest.iter().map(|l| l.to_string());
    lines.chain(rest).collect::<Vec<_>>().join("\n")
}

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
                        u.finish_delivery(&post, &result, &owner, now)
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

/// GitHub review mode never turns a worker's legacy protocol line into a Slack
/// approval. Keep the factual prose/links and explicitly label discarded claims.
pub fn github_summary(text: &str) -> String {
    let mut removed = false;
    let mut kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let upper = line.trim_start().to_ascii_uppercase();
            let legacy = upper.starts_with("SIGN-OFF") || upper.starts_with("VERDICT ");
            removed |= legacy;
            !legacy
        })
        .collect();
    if removed {
        kept.push(
            "Legacy Slack sign-off omitted; that line does not establish GitHub publication.",
        );
    }
    if removed {
        kept.join("\n")
    } else {
        text.to_owned()
    }
}
