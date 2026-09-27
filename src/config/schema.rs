use super::{
    registry::{Policy, Registry},
    Attention,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Owner {
    pub slack_user: String,
    pub profile: String,
    pub contract: Option<PathBuf>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Slack {
    pub workspace: String,
    pub channels: Vec<String>,
    pub delegate_channels: Option<Vec<String>>,
    pub app_token_env: String,
    pub user_token_env: String,
    pub general_messages: bool,
    pub cooldown: f64,
}
impl Default for Slack {
    fn default() -> Self {
        Self {
            workspace: String::new(),
            channels: vec![],
            delegate_channels: None,
            app_token_env: "SLACK_APP_TOKEN".into(),
            user_token_env: "SLACK_USER_TOKEN".into(),
            general_messages: true,
            cooldown: 60.,
        }
    }
}
impl Slack {
    pub fn may_delegate(&self, channel: &str) -> bool {
        self.delegate_channels
            .as_ref()
            .is_none_or(|v| v.iter().any(|s| s == channel))
    }
}
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
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GitHub {
    pub enabled: bool,
    pub token_env: String,
    pub cache_seconds: f64,
}
impl Default for GitHub {
    fn default() -> Self {
        Self {
            enabled: true,
            token_env: "FRIDICA_GITHUB_TOKEN".into(),
            cache_seconds: 180.,
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub path: PathBuf,
    pub control_socket: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub owner: Owner,
    pub slack: Slack,
    pub parent: Parent,
    pub limits: Limits,
    pub policy: Policy,
    pub machines: Registry,
    pub state: State,
    pub github: GitHub,
    pub attention: Attention,
    pub path: PathBuf,
    pub fingerprint: String,
}
impl Config {
    pub fn secret_env(&self) -> [&str; 3] {
        [
            &self.slack.app_token_env,
            &self.slack.user_token_env,
            &self.github.token_env,
        ]
    }
    /// Return only the configured names here; adapters read the actual credentials
    /// just before startup, and must not serialize them into configuration/replay.
    pub fn validate_tokens(&self, env: impl Fn(&str) -> Option<String>) -> anyhow::Result<()> {
        if !env(&self.slack.app_token_env).is_some_and(|s| s.starts_with("xapp-")) {
            anyhow::bail!(
                "{} must hold a Slack app-level token (xapp-...)",
                self.slack.app_token_env
            );
        }
        if !env(&self.slack.user_token_env).is_some_and(|s| s.starts_with("xoxp-")) {
            anyhow::bail!(
                "{} must hold a Slack user token (xoxp-...)",
                self.slack.user_token_env
            );
        }
        Ok(())
    }
}
