//! Host-owned native MCP binding. No secrets or project protocol in configuration.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const NATIVE_ENV: &str = "OVERLEAF_NATIVE_SESSION";
pub const NATIVE_ENTRY: &str = "fridica/overleaf/native-session";
pub const OPERATIONS: [&str; 7] = [
    "list_comments",
    "read_comment_thread",
    "add_comment",
    "reply_comment",
    "read_file",
    "list_tracked_changes",
    "find_and_replace",
];
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Installed thin adapter argv, e.g. ["python3", "/.../adapter.py", "--node", "/.../node"].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub native_adapter: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_recipient: Option<Recipient>,
    /// Explicit owner scope; absent scope denies forwarding even with a saved session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Login {
    pub resources: Option<std::path::PathBuf>,
    pub node: Option<std::path::PathBuf>,
    pub browser: Option<std::path::PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub expected_email: String,
    pub project: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}
impl Scope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            valid_id(&self.project) && valid_email(&self.expected_email),
            "native login requires an explicit account email and 24-character project ID"
        );
        ensure!(
            self.channel
                .as_deref()
                .is_none_or(|c| super::loader::slack_id(c, "CG")),
            "native channel must be an explicit Slack channel ID"
        );
        Ok(())
    }
}
fn valid_id(id: &str) -> bool {
    id.len() == 24
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn valid_email(email: &str) -> bool {
    !email.is_empty()
        && email.len() <= 254
        && email.is_ascii()
        && email.split('@').count() == 2
        && email.split('@').all(|v| !v.is_empty())
        && !email.bytes().any(|b| b <= 32 || b == 127)
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub worker_id: String,
    pub machine: String,
    pub workspace: String,
    /// Set only for an explicitly timed owner grant; otherwise actual session expiry/revocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
}
impl Recipient {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.worker_id
                .strip_prefix("worker-")
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .is_some(),
            "recipient requires a worker UUID"
        );
        ensure!(
            [&self.machine, &self.workspace]
                .iter()
                .all(|s| !s.is_empty() && !s.chars().any(char::is_control)),
            "native recipient requires an explicit registered machine and workspace"
        );
        ensure!(
            self.expires_at != Some(0),
            "invalid native recipient expiry"
        );
        Ok(())
    }
    pub fn permits(&self, worker: &str, machine: &str, workspace: &str) -> bool {
        self.validate().is_ok()
            && self.worker_id == worker
            && self.machine == machine
            && self.workspace == workspace
            && self.expires_at.is_none_or(|t| t > now())
    }
}
pub fn validate(config: &super::Config) -> Result<()> {
    ensure!(
        config
            .overleaf
            .native_adapter
            .iter()
            .all(|s| !s.is_empty() && !s.chars().any(char::is_control)),
        "native adapter argv contains an empty/control argument"
    );
    if let Some(scope) = &config.overleaf.native_scope {
        scope.validate()?;
        ensure!(
            scope
                .channel
                .as_ref()
                .is_none_or(|c| config.slack.channels.contains(c)),
            "native channel must be in the owner Slack allowlist"
        );
    }
    if let Some(r) = &config.overleaf.native_recipient {
        r.validate()?;
        ensure!(
            config.overleaf.enabled
                && !config.overleaf.native_adapter.is_empty()
                && config
                    .machines
                    .get(&r.machine)
                    .and_then(|m| m.workspace(&r.workspace))
                    .is_some(),
            "native recipient requires an enabled registered placement and installed adapter argv"
        );
    }
    Ok(())
}

/// Credential envelope only; Overleaf's protocol remains in the separate AGPL subprocess.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    schema: u8,
    cookie: String,
    account_id: String,
    expected_email: String,
    projects: Vec<String>,
    operations: Vec<String>,
    expires_at: Option<u64>,
    recipient: Option<Recipient>,
    channel: Option<String>,
}
pub(crate) fn validate_session(raw: &str) -> Result<()> {
    ensure!(raw.len() <= 16384, "native session too large");
    let s: Session =
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid native session"))?;
    ensure!(
        s.schema == 2
            && s.projects.len() == 1
            && valid_id(&s.projects[0])
            && s.operations == OPERATIONS
            && valid_id(&s.account_id)
            && valid_email(&s.expected_email)
            && s.expires_at.is_none_or(|t| t > now()),
        "native session outside scope or expired"
    );
    let mut names = std::collections::BTreeSet::new();
    for pair in s.cookie.split("; ") {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| anyhow::anyhow!("invalid native cookie"))?;
        ensure!(
            matches!(name, "overleaf_session2" | "GCLB")
                && names.insert(name)
                && !value.is_empty()
                && value.len() <= 4096
                && value
                    .bytes()
                    .all(|b| (33..127).contains(&b) && !b";\"\\,".contains(&b)),
            "invalid native cookie"
        );
    }
    ensure!(
        names.contains("overleaf_session2"),
        "native session cookie missing"
    );
    if let Some(r) = &s.recipient {
        r.validate()?;
    }
    ensure!(
        s.channel
            .as_deref()
            .is_none_or(|c| super::loader::slack_id(c, "CG")),
        "native channel mismatch"
    );
    Ok(())
}
pub(crate) fn validate_session_scope(raw: &str, scope: &Scope) -> Result<()> {
    validate_session(raw)?;
    scope.validate()?;
    let s: Session = serde_json::from_str(raw)?;
    ensure!(
        s.projects == [scope.project.clone()]
            && s.expected_email == scope.expected_email
            && s.channel
                .as_ref()
                .is_none_or(|c| Some(c) == scope.channel.as_ref()),
        "saved native session does not match configured account/project/channel"
    );
    Ok(())
}
pub(crate) fn bound_session(raw: &str, recipient: &Recipient, scope: &Scope) -> Result<String> {
    validate_session_scope(raw, scope)?;
    let channel = scope
        .channel
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("explicit native channel required"))?;
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    if let Some(deadline) = recipient.expires_at {
        value["expires_at"] = serde_json::json!(value["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    value["recipient"] = serde_json::to_value(recipient)?;
    value["channel"] = channel.as_str().into();
    let raw = value.to_string();
    validate_session(&raw)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    const PROJECT: &str = "333333333333333333333333";
    const CHANNEL: &str = "CEXAMPLE";
    #[test]
    fn grant_is_exact_and_lifetime_is_not_artificially_shortened() {
        let scope = Scope {
            expected_email: "owner@example.invalid".into(),
            project: PROJECT.into(),
            channel: Some(CHANNEL.into()),
        };
        let r = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        let s = serde_json::json!({"schema":2,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":[PROJECT], "operations":OPERATIONS});
        let b: serde_json::Value =
            serde_json::from_str(&bound_session(&s.to_string(), &r, &scope).unwrap()).unwrap();
        assert!(b["expires_at"].is_null());
        assert_eq!(b["channel"], CHANNEL);
        assert!(!r.permits(
            "worker-22222222-2222-4222-8222-222222222222",
            "dungeon3",
            "snapy"
        ));
        assert!(validate_session_scope(
            &s.to_string().replace(PROJECT, "222222222222222222222222"),
            &scope
        )
        .is_err());
        let mut timed = r;
        timed.expires_at = Some(now() + 60);
        let b: serde_json::Value =
            serde_json::from_str(&bound_session(&s.to_string(), &timed, &scope).unwrap()).unwrap();
        assert_eq!(b["expires_at"], timed.expires_at.unwrap());
    }
}
