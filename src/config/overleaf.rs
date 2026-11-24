//! Host-owned native MCP binding. No secrets or project protocol in configuration.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

pub const NATIVE_ENV: &str = "OVERLEAF_NATIVE_SESSION";
pub const NATIVE_ENTRY: &str = "fridica/overleaf/native-session";
pub const PROJECT: &str = "6b0483a71b304b7fd2141775";
pub const CHANNEL: &str = "C0CL1NW0DGS";
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
}
impl Settings {
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
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
            self.machine == "dungeon3" && self.workspace == "snapy",
            "native recipient must be dungeon3:snapy"
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
            && s.projects == [PROJECT]
            && s.operations == OPERATIONS
            && s.account_id.len() == 24
            && s.account_id.bytes().all(|b| b.is_ascii_hexdigit())
            && !s.expected_email.is_empty()
            && s.expected_email.len() <= 254
            && s.expected_email.is_ascii()
            && s.expected_email.split('@').count() == 2
            && s.expected_email.split('@').all(|v| !v.is_empty())
            && !s.expected_email.bytes().any(|b| b <= 32 || b == 127)
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
        s.channel.as_deref().is_none_or(|c| c == CHANNEL),
        "native channel mismatch"
    );
    Ok(())
}
pub(crate) fn bound_session(raw: &str, recipient: &Recipient) -> Result<String> {
    validate_session(raw)?;
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    if let Some(deadline) = recipient.expires_at {
        value["expires_at"] = serde_json::json!(value["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    value["recipient"] = serde_json::to_value(recipient)?;
    value["channel"] = CHANNEL.into();
    let raw = value.to_string();
    validate_session(&raw)?;
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grant_is_exact_and_lifetime_is_not_artificially_shortened() {
        let r = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        let s = serde_json::json!({"schema":2,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":[PROJECT], "operations":OPERATIONS});
        let b: serde_json::Value =
            serde_json::from_str(&bound_session(&s.to_string(), &r).unwrap()).unwrap();
        assert!(b["expires_at"].is_null());
        assert_eq!(b["channel"], CHANNEL);
        assert!(!r.permits(
            "worker-22222222-2222-4222-8222-222222222222",
            "dungeon3",
            "snapy"
        ));
        assert!(
            validate_session(&s.to_string().replace(PROJECT, "222222222222222222222222")).is_err()
        );
        let mut timed = r;
        timed.expires_at = Some(now() + 60);
        let b: serde_json::Value =
            serde_json::from_str(&bound_session(&s.to_string(), &timed).unwrap()).unwrap();
        assert_eq!(b["expires_at"], timed.expires_at.unwrap());
    }
}
