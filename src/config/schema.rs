use super::{
    registry::{Policy, Registry},
    Attention,
};
pub use fridica_core::config::{Limits, Parent, Placement};
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
            cache_seconds: 60.,
        }
    }
}
/// Checks on text before it is published.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Egress {
    /// A private (mode 0600) file of terms that must never be published.
    pub deny_list: Option<PathBuf>,
}
impl Egress {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
/// Interim progress notes posted while a job runs (#105). Fridica's own
/// section until these join `limits` in the next fridica-core release.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Progress {
    /// Seconds between reads of a running job's progress file; 0 turns notes off.
    pub interval: f64,
    /// Characters per posted note; longer notes are cut with a marker.
    pub chars: usize,
}
impl Default for Progress {
    fn default() -> Self {
        Self {
            interval: 120.,
            chars: 1500,
        }
    }
}
impl Progress {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
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
    #[serde(default, skip_serializing_if = "super::isolation::Settings::is_empty")]
    pub isolation: super::isolation::Settings,
    #[serde(default, skip_serializing_if = "Placement::is_default")]
    pub placement: Placement,
    #[serde(default, skip_serializing_if = "Egress::is_default")]
    pub egress: Egress,
    #[serde(default, skip_serializing_if = "Progress::is_default")]
    pub progress: Progress,
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
