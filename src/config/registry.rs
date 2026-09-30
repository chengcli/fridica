//! Resolved machine configuration. Model-facing payloads deliberately omit paths.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
};

pub const BACKENDS: [&str; 2] = ["claude", "codex"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Policy {
    pub mode: String,
    pub network: Vec<String>,
    pub approvals: String,
    pub fetch_repos: Vec<String>,
    pub approval_timeout: f64,
    pub auto_approve: Vec<String>,
    pub auto_deny: Vec<String>,
    pub gpu_confine: Option<bool>,
    pub claude_prompts: String,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            mode: "write".into(),
            network: vec![],
            approvals: "auto".into(),
            fetch_repos: vec![],
            approval_timeout: 1800.,
            auto_approve: vec![],
            auto_deny: vec![],
            gpu_confine: None,
            claude_prompts: "host".into(),
        }
    }
}
impl Policy {
    /// The granted spelling of `repo`, matched case-insensitively (Unicode case
    /// folding, so look-alike letters cannot bypass a grant).
    pub fn fetch_grant(&self, repo: &str) -> Option<&str> {
        use unicode_casefold::UnicodeCaseFold;
        self.fetch_repos
            .iter()
            .find(|r| r.as_str().case_fold().eq(repo.case_fold()))
            .map(String::as_str)
    }
    pub fn validate(&self) -> Result<()> {
        if !["read-only", "write", "full"].contains(&self.mode.as_str()) {
            bail!("policy.mode must be read-only, write or full");
        }
        if !["never", "on-request", "untrusted", "auto"].contains(&self.approvals.as_str()) {
            bail!("invalid policy.approvals");
        }
        if !["host", "none"].contains(&self.claude_prompts.as_str()) {
            bail!("policy.claude_prompts must be host or none");
        }
        if !self.approval_timeout.is_finite() || self.approval_timeout <= 0. {
            bail!("policy.approval_timeout must be positive");
        }
        for list in [
            &self.network,
            &self.fetch_repos,
            &self.auto_approve,
            &self.auto_deny,
        ] {
            if list.iter().any(String::is_empty) {
                bail!("policy lists require nonempty strings");
            }
        }
        if self.network.iter().any(|s| !domain(s)) {
            bail!("policy.network must contain host names or *.patterns");
        }
        if self.fetch_repos.iter().any(|s| !github_repo(s)) {
            bail!("policy.fetch_repos must contain GitHub owner/repo names");
        }
        if self
            .fetch_repos
            .iter()
            .map(|s| s.to_ascii_lowercase())
            .collect::<HashSet<_>>()
            .len()
            != self.fetch_repos.len()
        {
            bail!("policy.fetch_repos contains a duplicate repository");
        }
        Ok(())
    }
    pub fn override_with(&self, data: Value) -> Result<Self> {
        let Value::Object(overrides) = data else {
            bail!("policy must be a table");
        };
        let mut base = serde_json::to_value(self)?;
        base.as_object_mut().unwrap().extend(overrides);
        let policy: Self = serde_json::from_value(base)?;
        policy.validate()?;
        Ok(policy)
    }
    pub fn any_network(&self) -> bool {
        self.network.iter().any(|s| s == "*")
    }
}

pub fn name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}
fn domain(s: &str) -> bool {
    if s == "*" {
        return true;
    }
    let s = s.strip_prefix("*.").unwrap_or(s);
    s.split('.').all(|part| {
        !part.is_empty()
            && part.len() <= 63
            && part.as_bytes()[0].is_ascii_alphanumeric()
            && part.as_bytes()[part.len() - 1].is_ascii_alphanumeric()
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}
pub fn github_repo(s: &str) -> bool {
    let Some((owner, repo)) = s.split_once('/') else {
        return false;
    };
    !owner.is_empty()
        && owner.len() <= 39
        && owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !repo.is_empty()
        && repo.len() <= 100
        && ![".", ".."].contains(&repo)
        && repo
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}
pub fn valid_fetch_ref(s: &str) -> bool {
    let kind = s == "HEAD"
        || ((40..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_hexdigit()))
        || s.strip_prefix("refs/heads/").is_some_and(|branch| {
            !branch.is_empty()
                && branch.as_bytes()[0].is_ascii_alphanumeric()
                && branch
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._/-".contains(&b))
        })
        || s.strip_prefix("refs/pull/")
            .and_then(|s| s.strip_suffix("/head"))
            .is_some_and(|id| {
                !id.is_empty()
                    && id.as_bytes()[0].is_ascii_digit()
                    && id.as_bytes()[0] != b'0'
                    && id.bytes().all(|b| b.is_ascii_digit())
            });
    kind && !s.contains("..")
        && !s.contains("//")
        && !s.ends_with(['/', '.'])
        && !s.ends_with(".lock")
}
pub fn ssh_host(s: &str) -> bool {
    let parts: Vec<_> = s.split('@').collect();
    if parts.len() > 2 {
        return false;
    }
    if parts.len() == 2 {
        let user = parts[0];
        if user.is_empty()
            || !(user.as_bytes()[0].is_ascii_alphanumeric() || user.starts_with('_'))
            || !user
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return false;
        }
    }
    let host = parts[parts.len() - 1];
    !host.is_empty()
        && host.as_bytes()[0].is_ascii_alphanumeric()
        && host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Resources {
    pub cpus: Option<usize>,
    pub gpus: Option<Vec<usize>>,
    pub gpu_type: String,
    pub memory_gb: Option<f64>,
    pub notes: String,
}
impl Resources {
    pub fn validate(&self) -> Result<()> {
        if self.cpus == Some(0) {
            bail!("resources.cpus must be positive");
        }
        if self.memory_gb.is_some_and(|n| !n.is_finite() || n <= 0.) {
            bail!("resources.memory_gb must be positive");
        }
        if self
            .gpus
            .as_ref()
            .is_some_and(|v| v.iter().collect::<HashSet<_>>().len() != v.len())
        {
            bail!("resources.gpus lists a device twice");
        }
        if self.gpu_type.chars().count() > 1000 || self.notes.chars().count() > 1000 {
            bail!("resource strings must have at most 1000 characters");
        }
        Ok(())
    }
    pub fn payload(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .retain(|_, v| !v.is_null() && v != "");
        value
    }
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut vars = BTreeMap::new();
        if let Some(cpus) = self.cpus {
            vars.insert("OMP_NUM_THREADS".into(), cpus.to_string());
        }
        if let Some(gpus) = &self.gpus {
            vars.insert(
                "CUDA_VISIBLE_DEVICES".into(),
                gpus.iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        vars
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub name: String,
    pub path: PathBuf,
    pub policy: Policy,
    pub subfolders: bool,
}
impl Workspace {
    pub fn writable(&self) -> bool {
        self.policy.mode != "read-only"
    }
    pub fn for_slot(&self, slot: usize) -> Self {
        let mut view = self.clone();
        if self.subfolders && slot > 0 {
            view.path.push(format!("worker{slot}"));
        }
        view
    }
}
pub fn slot_gpus(gpus: &Option<Vec<usize>>, slot: usize, slots: usize) -> Option<Vec<usize>> {
    let Some(gpus) = gpus else {
        return None;
    };
    if gpus.is_empty() || slots <= 1 || slot < 1 {
        return Some(gpus.clone());
    }
    if gpus.len() < slots {
        return Some(vec![gpus[(slot - 1) % gpus.len()]]);
    }
    let share = gpus.len() / slots;
    let start = (slot - 1).saturating_mul(share).min(gpus.len());
    let end = slot.saturating_mul(share).min(gpus.len());
    Some(gpus[start..end].to_vec())
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Slurm {
    pub account: String,
    pub partition: String,
    pub gres: String,
    pub time: String,
    pub extra: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub name: String,
    pub transport: String,
    pub workspaces: Vec<Workspace>,
    pub backends: Vec<String>,
    pub default_backend: String,
    pub policy: Policy,
    pub host: String,
    pub tags: Vec<String>,
    pub resources: Resources,
    pub max_workers: usize,
    pub max_jobs: usize,
    pub slurm: Option<Slurm>,
    pub description: String,
}
impl Machine {
    pub fn remote(&self) -> bool {
        self.transport != "local"
    }
    pub fn workspace(&self, name: &str) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.name == name)
    }
    pub fn for_slot(&self, slot: usize) -> Self {
        let mut view = self.clone();
        view.resources.gpus = slot_gpus(&self.resources.gpus, slot, self.max_jobs);
        view
    }
    pub fn payload(&self, busy: usize) -> Value {
        let workspaces: BTreeMap<_, _> = self
            .workspaces
            .iter()
            .map(|w| (&w.name, &w.policy.mode))
            .collect();
        let fetch_repos: BTreeMap<_, _> = self
            .workspaces
            .iter()
            .filter(|w| !w.policy.fetch_repos.is_empty())
            .map(|w| (&w.name, &w.policy.fetch_repos))
            .collect();
        let mut result = json!({"name":self.name,"tags":self.tags,"backends":self.backends,"default_backend":self.default_backend,
            "workspaces":workspaces,"fetch_repos":fetch_repos,"resources":self.resources.payload(),"busy_jobs":busy,"max_jobs":self.max_jobs});
        if !self.description.is_empty() {
            result["description"] = json!(self.description);
        }
        result
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub machines: Vec<Machine>,
    pub default: String,
}
impl Registry {
    pub fn validate(&self) -> Result<()> {
        if self.machines.is_empty() {
            bail!("configure at least one machine");
        }
        if self.names().iter().collect::<HashSet<_>>().len() != self.machines.len() {
            bail!("machine names must be unique");
        }
        if self.get(&self.default).is_none() {
            bail!("default machine {:?} is not configured", self.default);
        }
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.name == name)
    }
    pub fn names(&self) -> Vec<String> {
        self.machines.iter().map(|m| m.name.clone()).collect()
    }
    pub fn payload(&self, busy: &BTreeMap<String, usize>) -> Vec<Value> {
        self.machines
            .iter()
            .map(|m| m.payload(*busy.get(&m.name).unwrap_or(&0)))
            .collect()
    }
}
