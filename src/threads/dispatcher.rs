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
        self.drain_with(limit, deny, None).await
    }
    /// Like `drain_checked`, also verifying PR or branch sign-offs against
    /// a fresh GitHub head through `heads`. A sign-off that
    /// cannot be verified (no GitHub reader, no repository, or a failed read)
    /// is not sent, like one that names a stale head.
    pub async fn drain_with(
        &self,
        limit: usize,
        deny: Option<&Path>,
        heads: Option<&dyn crate::github::client::Api>,
    ) -> Result<usize> {
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
                let refused = match check(&post, &rules) {
                    Some(rule) => Some(rule),
                    None => self.signoffs(&post, heads, &rules).await?,
                };
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
impl<D: Delivery> Dispatcher<D> {
    /// The rule a post's sign-off lines break, if any. A sign-off clears a
    /// pull request for merge, so the PR's title, body and commit messages,
    /// read back from GitHub, must also pass the egress rules.
    async fn signoffs(
        &self,
        post: &ClaimedPost,
        heads: Option<&dyn crate::github::client::Api>,
        rules: &DenyList,
    ) -> Result<Option<String>> {
        let Some(lines) = signoff_lines(&post.post.text) else {
            return Ok(Some("signoff_malformed".into()));
        };
        if lines.is_empty() {
            return Ok(None);
        }
        let Some(heads) = heads else {
            return Ok(Some("signoff_unverified".into()));
        };
        let session = post.post.session_id.clone();
        let context_repo: String = self
            .store
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT COALESCE(json_extract(context_json,'$.repo'),'') FROM threads WHERE id=?",
                    [session],
                    |r| r.get(0),
                )
                .unwrap_or_default())
            })
            .await?;
        use crate::github::client::{Failure, Operation, Request};
        for line in lines {
            let repo = line.repo.unwrap_or_else(|| context_repo.clone());
            if !crate::github::client::repository(&repo) {
                return Ok(Some("signoff_repo_unknown".into()));
            }
            let read = |operation| {
                heads.get(Request {
                    repo: repo.clone(),
                    operation,
                })
            };
            if let Some(branch) = line.branch {
                let reference = match read(Operation::Branch { name: branch }).await {
                    Ok(reference) => reference,
                    Err(Failure::Recording) => anyhow::bail!("sign-off check recording failed"),
                    Err(_) => return Ok(Some("signoff_unverified".into())),
                };
                let head = reference["object"]["sha"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if head.len() < 40 || !head.starts_with(&line.sha) {
                    return Ok(Some("signoff_stale_head".into()));
                }
                let commit = match read(Operation::Tree { head }).await {
                    Ok(commit) => commit,
                    Err(Failure::Recording) => anyhow::bail!("sign-off check recording failed"),
                    Err(_) => return Ok(Some("signoff_unverified".into())),
                };
                let Some(message) = commit["message"].as_str() else {
                    return Ok(Some("signoff_unverified".into()));
                };
                if let Some(rule) = egress::scan(message, rules) {
                    return Ok(Some(format!("signoff_branch_{rule}")));
                }
                continue;
            }
            let pull = match read(Operation::Pull {
                number: line.number,
            })
            .await
            {
                Ok(pull) => pull,
                Err(Failure::Recording) => anyhow::bail!("sign-off check recording failed"),
                Err(_) => return Ok(Some("signoff_unverified".into())),
            };
            let head = pull["head"]["sha"]
                .as_str()
                .unwrap_or("")
                .to_ascii_lowercase();
            if head.len() < 40 || !head.starts_with(&line.sha) {
                return Ok(Some("signoff_stale_head".into()));
            }
            let base = pull["base"]["sha"].as_str().unwrap_or("").to_string();
            if !crate::github::client::sha(&base) {
                return Ok(Some("signoff_unverified".into()));
            }
            let compare = match read(Operation::Compare { base, head }).await {
                Ok(compare) => compare,
                Err(Failure::Recording) => anyhow::bail!("sign-off check recording failed"),
                Err(_) => return Ok(Some("signoff_unverified".into())),
            };
            // GitHub lists at most 250 commits; a longer PR cannot be read back.
            let Some(commits) = compare["commits"]
                .as_array()
                .filter(|commits| compare["total_commits"].as_u64() == Some(commits.len() as u64))
            else {
                return Ok(Some("signoff_unverified".into()));
            };
            let texts = [&pull["title"], &pull["body"]]
                .into_iter()
                .chain(commits.iter().map(|c| &c["commit"]["message"]))
                .filter_map(serde_json::Value::as_str);
            for text in texts {
                if let Some(rule) = egress::scan(text, rules) {
                    return Ok(Some(format!("signoff_pr_{rule}")));
                }
            }
        }
        Ok(None)
    }
}
struct Signoff {
    repo: Option<String>,
    branch: Option<String>,
    number: u64,
    sha: String,
}
/// `SIGN-OFF [owner/repo[@branch]] #<number> <sha> ...` lines (provision04).
/// A preceding `TARGET owner/repo@branch #<number>` keeps the sign-off line bare
/// for consumers that require that format, while retaining a verifiable ref.
fn signoff_lines(text: &str) -> Option<Vec<Signoff>> {
    static LINE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^SIGN-OFF (?:(?P<repo>[A-Za-z0-9-]+/[A-Za-z0-9._-]+)(?:@(?P<branch>[A-Za-z0-9._/-]+))? )?#(?P<number>[1-9][0-9]{0,9}) (?P<sha>[0-9a-f]{7,40}) (approve|approve \(code review\)|changes)$")
            .unwrap()
    });
    static TARGET: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"^TARGET (?P<repo>[A-Za-z0-9-]+/[A-Za-z0-9._-]+)@(?P<branch>[A-Za-z0-9._/-]+) #(?P<number>[1-9][0-9]{0,9})$")
            .unwrap()
    });
    let mut previous = "";
    let mut lines = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.to_ascii_uppercase().starts_with("SIGN-OFF") {
            let c = LINE.captures(line)?;
            let number = c.name("number")?.as_str().parse().ok()?;
            let mut repo = c.name("repo").map(|m| m.as_str().to_string());
            let mut branch = c.name("branch").map(|m| m.as_str().to_string());
            if previous.starts_with("TARGET ") {
                let target = TARGET.captures(previous)?;
                if repo.is_some() || target.name("number")?.as_str().parse::<u64>().ok()? != number
                {
                    return None;
                }
                repo = Some(target.name("repo")?.as_str().to_string());
                branch = Some(target.name("branch")?.as_str().to_string());
            }
            lines.push(Signoff {
                repo,
                branch,
                number,
                sha: c.name("sha")?.as_str().to_string(),
            });
        }
        previous = line;
    }
    Some(lines)
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
