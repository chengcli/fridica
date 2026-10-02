//! Fridica's domain configuration: parent, limits, placement and attention
//! settings, and the machine registry. Loading, paths and transport settings
//! belong to the host.
pub mod registry;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Parent {
    pub backend: String,
    pub model: String,
    pub triage_model: String,
    pub reasoning_effort: String,
    pub timeout: f64,
    pub context_chars: usize,
    pub default_machine: String,
    pub repos: Option<PathBuf>,
}
impl Default for Parent {
    fn default() -> Self {
        Self {
            backend: "claude".into(),
            model: String::new(),
            triage_model: String::new(),
            reasoning_effort: String::new(),
            timeout: 180.,
            context_chars: 24000,
            default_machine: String::new(),
            repos: None,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_delegations_per_turn: usize,
    pub max_workers_per_thread: usize,
    pub max_jobs: usize,
    pub parent_concurrency: usize,
    pub job_timeout: f64,
    pub worker_idle: f64,
    pub session_timeout: f64,
    pub auto_resume: bool,
    pub reply_chars: usize,
    pub report_fast_path: bool,
    /// Thread context a forked worker is given on its first job, in characters.
    pub worker_context_chars: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_delegations_per_turn: 3,
            max_workers_per_thread: 4,
            max_jobs: 4,
            parent_concurrency: 4,
            job_timeout: 14400.,
            worker_idle: 1800.,
            session_timeout: 1209600.,
            auto_resume: false,
            reply_chars: 7000,
            report_fast_path: true,
            worker_context_chars: 12000,
        }
    }
}
/// Load-aware placement: probe candidate machines before choosing among them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Placement {
    pub probe: bool,
    /// Seconds a machine reading is reused before probing again.
    pub probe_ttl: f64,
    /// Seconds one probe may take, including the SSH connection.
    pub probe_timeout: f64,
    /// 1-minute load average per declared CPU at which a machine is saturated.
    pub max_load: f64,
    /// GPU utilization percent, or used memory fraction, at which a GPU is in use.
    pub max_gpu_utilization: f64,
    pub max_gpu_memory: f64,
}
impl Default for Placement {
    fn default() -> Self {
        Self {
            probe: true,
            probe_ttl: 60.,
            probe_timeout: 10.,
            max_load: 1.0,
            max_gpu_utilization: 90.,
            max_gpu_memory: 0.9,
        }
    }
}
impl Placement {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Attention {
    pub mention_grace: f64,
    pub max_replies_per_hour: usize,
    pub max_echo_replies_per_hour: usize,
    pub streak_signal: usize,
}
impl Default for Attention {
    fn default() -> Self {
        Self {
            mention_grace: 900.,
            max_replies_per_hour: 20,
            max_echo_replies_per_hour: 10,
            streak_signal: 3,
        }
    }
}
impl Attention {
    pub fn validate(&self) -> Result<()> {
        if !self.mention_grace.is_finite()
            || self.mention_grace <= 0.
            || self.streak_signal == 0
            || self.max_replies_per_hour == 0
            || self.max_echo_replies_per_hour == 0
        {
            bail!("attention limits must be positive");
        }
        Ok(())
    }
}
