//! Typed owner-credential command construction. No model-generated command or
//! endpoint is accepted. There is deliberately no merge/protection operation.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub host: String,
    pub upstream: String,
    pub fork: String,
    pub account: String,
}
fn component(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn repo(s: &str) -> bool {
    let parts: Vec<_> = s.split('/').collect();
    parts.len() == 2 && parts.iter().all(|s| component(s))
}
impl Repository {
    pub fn validate(&self) -> Result<()> {
        if !component(&self.host)
            || !component(&self.account)
            || !repo(&self.upstream)
            || !repo(&self.fork)
        {
            bail!("invalid explicit GitHub host/account/repository mapping");
        }
        if self.upstream.eq_ignore_ascii_case(&self.fork) {
            bail!("fork destination must differ from upstream");
        }
        Ok(())
    }
}
pub fn sha(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}
pub fn branch(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('.')
        && !s.contains("..")
        && !s.contains("//")
        && s.split('/')
            .all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b))
}

#[derive(Debug, Clone)]
pub enum ApiOperation {
    Identity,
    Pull { number: u64 },
    Checks { commit: String },
    Reviews { number: u64 },
    Comment { number: u64, body: String },
    RequestReview { number: u64, reviewer: String },
}

/// `gh api` arguments for one operation. A comment is published text, so it
/// passes the egress check first; a hit names the rule, never the term.
pub fn gh_arguments(
    mapping: &Repository,
    operation: ApiOperation,
    deny: &fridica_core::egress::DenyList,
) -> Result<Vec<String>> {
    mapping.validate()?;
    if let ApiOperation::Comment { body, .. } = &operation {
        if let Some(rule) = fridica_core::egress::scan(body, deny) {
            bail!("comment refused by the egress check ({rule})");
        }
    }
    let base = format!("repos/{}", mapping.upstream);
    let (method, endpoint, fields) = match operation {
        ApiOperation::Identity => ("GET", "user".into(), vec![]),
        ApiOperation::Pull { number } | ApiOperation::Reviews { number } if number == 0 => {
            bail!("invalid pull number")
        }
        ApiOperation::Pull { number } => ("GET", format!("{base}/pulls/{number}"), vec![]),
        ApiOperation::Checks { commit } => {
            if !sha(&commit) {
                bail!("invalid commit");
            }
            ("GET", format!("{base}/commits/{commit}/check-runs"), vec![])
        }
        ApiOperation::Reviews { number } => {
            ("GET", format!("{base}/pulls/{number}/reviews"), vec![])
        }
        ApiOperation::Comment { number, body } => {
            if number == 0 || body.trim().is_empty() {
                bail!("invalid comment");
            }
            (
                "POST",
                format!("{base}/issues/{number}/comments"),
                vec![format!("body={body}")],
            )
        }
        ApiOperation::RequestReview { number, reviewer } => {
            if number == 0 || !component(&reviewer) {
                bail!("invalid reviewer");
            }
            (
                "POST",
                format!("{base}/pulls/{number}/requested_reviewers"),
                vec![format!("reviewers[]={reviewer}")],
            )
        }
    };
    let mut args = vec![
        "api".into(),
        "--hostname".into(),
        mapping.host.clone(),
        "--method".into(),
        method.into(),
        endpoint,
    ];
    for field in fields {
        args.extend(["--raw-field".into(), field]);
    }
    Ok(args)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedPush {
    pub repository: Repository,
    pub branch: String,
    pub expected_remote_head: String,
    pub commit: String,
    pub tree: String,
}
impl PreparedPush {
    pub fn arguments(&self, verified_commit: &str, verified_tree: &str) -> Result<Vec<String>> {
        self.repository.validate()?;
        if !branch(&self.branch)
            || !sha(&self.expected_remote_head)
            || !sha(&self.commit)
            || !sha(&self.tree)
        {
            bail!("push requires a valid branch and explicit expected head, commit, and tree");
        }
        if verified_commit != self.commit || verified_tree != self.tree {
            bail!("prepared commit/tree mismatch");
        }
        // Invoke ONLY in a private executor-created bare repository. Never run
        // these credentialed arguments in a worker-controlled checkout.
        Ok(vec![
            "-c".into(),
            "core.hooksPath=/dev/null".into(),
            "-c".into(),
            "protocol.allow=never".into(),
            "-c".into(),
            "protocol.ssh.allow=always".into(),
            "push".into(),
            "--porcelain".into(),
            "--no-verify".into(),
            format!(
                "--force-with-lease=refs/heads/{}:{}",
                self.branch, self.expected_remote_head
            ),
            "--".into(),
            format!(
                "ssh://git@{}/{}.git",
                self.repository.host, self.repository.fork
            ),
            format!("{}:refs/heads/{}", self.commit, self.branch),
        ])
    }
}
