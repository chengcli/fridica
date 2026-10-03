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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    pub path: PathBuf,
    pub control_socket: PathBuf,
    /// Days a finished thread stays quiet before it moves to its weekly
    /// archive (#114); completed replay events older than this move too.
    /// 0 keeps everything in the live database.
    #[serde(
        default = "archive_after_days",
        skip_serializing_if = "is_archive_default"
    )]
    pub archive_after_days: f64,
    /// Hours a completed replay event stays in the live database before it
    /// moves to its week's archive. Events are the bulk of the database, so
    /// they leave sooner than threads. 0 keeps them.
    #[serde(
        default = "archive_events_after_hours",
        skip_serializing_if = "is_events_default"
    )]
    pub archive_events_after_hours: f64,
    /// What the replay ledger keeps of Slack responses (#116): `summary`
    /// (outcome and shape) or `full` (complete bodies, for debugging).
    #[serde(default = "record_mode", skip_serializing_if = "is_record_default")]
    pub record: String,
}
fn record_mode() -> String {
    "summary".into()
}
fn is_record_default(mode: &String) -> bool {
    *mode == record_mode()
}
fn archive_events_after_hours() -> f64 {
    24.
}
fn is_events_default(hours: &f64) -> bool {
    *hours == archive_events_after_hours()
}
fn archive_after_days() -> f64 {
    7.
}
fn is_archive_default(days: &f64) -> bool {
    *days == archive_after_days()
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
