//! Host-owned native MCP binding. No secrets or project protocol in configuration.
use anyhow::{ensure, Result};
use fridica_agent::overleaf::{
    login::normalize_base_url, native_operation, native_operations, native_tool_is_mutation,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) const MAX_SESSION_BYTES: usize = 16384;
pub const NATIVE_ENV: &str = "OVERLEAF_NATIVE_SESSION";
pub const NATIVE_ENTRY: &str = "fridica/overleaf/native-session";
pub(crate) const DEFAULT_ORIGIN: &str = "https://www.overleaf.com";
fn default_base_url() -> String {
    DEFAULT_ORIGIN.into()
}
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
    /// Deprecated, retained only so existing configurations remain readable. Never executed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub native_adapter: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_recipient: Option<Recipient>,
    /// Verified account and optional legacy limits; absent scope denies native forwarding.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_scope: Option<Scope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub login: Option<Login>,
}
impl Settings {
    pub(crate) fn origin(&self) -> &str {
        self.login
            .as_ref()
            .and_then(|login| login.base_url.as_deref())
            .unwrap_or(DEFAULT_ORIGIN)
    }

    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Login {
    /// Deprecated; native collaboration is bundled Rust.
    pub resources: Option<std::path::PathBuf>,
    /// Deprecated; no Node executable is used.
    pub node: Option<std::path::PathBuf>,
    pub browser: Option<std::path::PathBuf>,
    /// Explicit Overleaf origin; absent retains the cloud service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    pub expected_email: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}
impl Scope {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.project.as_deref().is_none_or(valid_id) && valid_email(&self.expected_email),
            "native login requires an account email and, if restricted, a 24-character project ID"
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
pub(crate) fn valid_id(id: &str) -> bool {
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
    if let Some(base_url) = config
        .overleaf
        .login
        .as_ref()
        .and_then(|login| login.base_url.as_deref())
    {
        normalize_base_url(base_url)?;
    }
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
                && config
                    .machines
                    .get(&r.machine)
                    .and_then(|m| m.workspace(&r.workspace))
                    .is_some(),
            "native recipient requires an enabled registered placement"
        );
    }
    Ok(())
}

/// Fridica account and task grant; Rust Interleaf handles the native protocol.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Session {
    schema: u8,
    #[serde(default = "default_base_url")]
    pub(crate) base_url: String,
    pub(crate) cookie: String,
    pub(crate) account_id: String,
    pub(crate) expected_email: String,
    pub(crate) projects: Option<Vec<String>>,
    pub(crate) operations: Vec<String>,
    expires_at: Option<u64>,
    recipient: Option<Recipient>,
    channel: Option<String>,
    #[serde(default)]
    pub(crate) legacy_workspaces: Vec<std::path::PathBuf>,
}
pub(crate) fn validate_session(raw: &str) -> Result<()> {
    parse_session(raw).map(|_| ())
}

fn parse_session(raw: &str) -> Result<Session> {
    ensure!(raw.len() <= MAX_SESSION_BYTES, "native session too large");
    let mut s: Session =
        serde_json::from_str(raw).map_err(|_| anyhow::anyhow!("invalid native session"))?;
    let origin = normalize_base_url(&s.base_url)?;
    let catalog = native_operations();
    let operations: BTreeSet<_> = s.operations.iter().collect();
    ensure!(
        matches!(s.schema, 2..=4)
            && (s.schema != 2 || s.projects.as_ref().is_some_and(|p| p.len() == 1))
            && s.projects.as_ref().is_none_or(|projects| {
                projects.len() <= 500
                    && (s.schema >= 4 || !projects.is_empty())
                    && projects.iter().all(|p| valid_id(p))
                    && projects.iter().collect::<BTreeSet<_>>().len() == projects.len()
            })
            && (if s.schema >= 4 {
                operations.len() == s.operations.len()
                    && s.operations
                        .iter()
                        .all(|op| op == "direct_edit" || catalog.contains(op))
            } else {
                s.operations == OPERATIONS && origin == default_base_url()
            })
            && valid_id(&s.account_id)
            && valid_email(&s.expected_email)
            && s.expires_at.is_none_or(|t| t > now()),
        "native session outside scope or expired"
    );
    ensure!(
        s.legacy_workspaces.len() <= 128
            && s.legacy_workspaces
                .iter()
                .all(|p| (p.is_absolute() || p.starts_with("~"))
                    && !p.components().any(|c| c == std::path::Component::ParentDir)
                    && !p.to_string_lossy().chars().any(char::is_control)),
        "invalid legacy receipt workspace"
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
    s.base_url = origin;
    Ok(s)
}

pub(crate) fn full_operations() -> Vec<String> {
    let mut ops = native_operations();
    ops.push("direct_edit".into());
    ops
}

/// Explicit owner action: expand operations while retaining every other saved limit.
pub(crate) fn upgrade_operations(raw: &str) -> Result<String> {
    let saved = parse_session(raw)?;
    ensure!(
        saved.schema >= 3 || (saved.channel.is_some() && saved.recipient.is_some()),
        "legacy native session requires explicit channel and recipient grants before an operation upgrade"
    );
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    value["schema"] = 4.into();
    value["operations"] = serde_json::json!(full_operations());
    let raw = value.to_string();
    validate_session(&raw)?;
    Ok(raw)
}

pub(crate) fn validate_session_scope(raw: &str, scope: &Scope) -> Result<Session> {
    scope.validate()?;
    let s = parse_session(raw)?;
    ensure!(
        s.expected_email == scope.expected_email
            && scope
                .project
                .as_ref()
                .is_none_or(|p| s.projects.as_ref().is_none_or(|ids| ids.contains(p)))
            && s.channel
                .as_ref()
                .is_none_or(|c| scope.channel.as_ref().is_none_or(|limit| c == limit)),
        "saved native session does not match configured account/project/channel"
    );
    Ok(s)
}
/// A new account-wide login cannot erase a legacy grant stored with the session.
pub(crate) fn renew_session(previous: &[u8], raw: &str) -> Result<String> {
    let incoming = parse_session(raw)?;
    let previous_raw = std::str::from_utf8(previous)
        .map_err(|_| anyhow::anyhow!("invalid saved session; explicit revocation required"))?;
    let old = parse_session(previous_raw)?;
    ensure!(
        old.schema != 2 || (old.channel.is_some() && old.recipient.is_some()),
        "legacy native session requires explicit channel and recipient grants"
    );
    let mut new: serde_json::Value = serde_json::from_str(raw)?;
    ensure!(
        old.base_url == incoming.base_url,
        "native origin changed; explicit revocation required"
    );
    new["base_url"] = old.base_url.into();
    new["schema"] = old.schema.max(incoming.schema).into();
    new["operations"] = serde_json::json!(old
        .operations
        .iter()
        .filter(|op| incoming.operations.contains(op))
        .collect::<Vec<_>>());
    let legacy: BTreeSet<_> = old
        .legacy_workspaces
        .iter()
        .chain(incoming.legacy_workspaces.iter())
        .collect();
    if !legacy.is_empty() {
        new["legacy_workspaces"] = serde_json::json!(legacy);
    }
    ensure!(
        old.account_id == new["account_id"] && old.expected_email == new["expected_email"],
        "native account changed; explicit revocation required"
    );
    if let Some(projects) = old.projects {
        let permitted: Vec<_> = projects
            .into_iter()
            .filter(|p| {
                new["projects"]
                    .as_array()
                    .is_none_or(|ids| ids.iter().any(|id| id == p))
            })
            .collect();
        new["projects"] = serde_json::json!(permitted);
    } else {
        ensure!(
            old.schema >= 3,
            "invalid legacy grant; explicit revocation required"
        );
    }
    if let Some(channel) = old.channel {
        ensure!(
            new["channel"].is_null() || new["channel"] == channel,
            "native channel changed; explicit revocation required"
        );
        new["channel"] = channel.into();
    }
    if let Some(recipient) = old.recipient {
        let saved = serde_json::to_value(recipient)?;
        ensure!(
            new["recipient"].is_null() || new["recipient"] == saved,
            "native recipient changed; explicit revocation required"
        );
        new["recipient"] = saved;
    }
    if let Some(deadline) = old.expires_at {
        new["expires_at"] = serde_json::json!(new["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    let renewed = new.to_string();
    validate_session(&renewed)?;
    Ok(renewed)
}

pub(crate) fn bound_session(
    raw: &str,
    recipient: &Recipient,
    configured_recipient: Option<&Recipient>,
    scope: &Scope,
    channel: &str,
    origin: &str,
    workspaces: impl IntoIterator<Item = std::path::PathBuf>,
) -> Result<String> {
    let saved = validate_session_scope(raw, scope)?;
    ensure!(
        saved.base_url == normalize_base_url(origin)?,
        "saved native origin differs from configured origin"
    );
    recipient.validate()?;
    // Schema2 predates automatic admission. An absent legacy grant was a denial,
    // not permission to bind the saved login to any newly admitted worker.
    ensure!(
        saved.schema >= 3
            || (scope.channel.is_some()
                && configured_recipient.is_some_and(|r| r.permits(
                    &recipient.worker_id,
                    &recipient.machine,
                    &recipient.workspace
                ))),
        "legacy native session requires configured recipient and channel grants"
    );
    ensure!(
        super::loader::slack_id(channel, "CG")
            && scope.channel.as_deref().is_none_or(|c| c == channel)
            && saved.channel.as_deref().is_none_or(|c| c == channel)
            && saved.recipient.as_ref().is_none_or(|r| r.permits(
                &recipient.worker_id,
                &recipient.machine,
                &recipient.workspace
            )),
        "saved native session does not permit this recipient/channel"
    );
    let mut value: serde_json::Value = serde_json::from_str(raw)?;
    if let Some(project) = &scope.project {
        value["projects"] = serde_json::json!([project]);
    }
    for deadline in [
        recipient.expires_at,
        saved.recipient.and_then(|r| r.expires_at),
    ]
    .into_iter()
    .flatten()
    {
        value["expires_at"] = serde_json::json!(value["expires_at"]
            .as_u64()
            .map_or(deadline, |t| t.min(deadline)));
    }
    let legacy: BTreeSet<_> = saved
        .legacy_workspaces
        .into_iter()
        .chain(workspaces)
        .collect();
    value["legacy_workspaces"] = serde_json::json!(legacy);
    value["recipient"] = serde_json::to_value(recipient)?;
    value["channel"] = channel.into();
    let raw = value.to_string();
    validate_session(&raw)?;
    Ok(raw)
}

/// Validate the exact target context at startup and again before each native call.
pub(crate) fn task_session(
    raw: &str,
    worker: &str,
    channel: &str,
    project: &str,
) -> Result<Session> {
    let session = parse_session(raw)?;
    ensure!(
        (project == "unbound"
            || (valid_id(project)
                && session
                    .projects
                    .as_ref()
                    .is_some_and(|ids| ids.len() == 1 && ids[0] == project)))
            && session.channel.as_deref() == Some(channel)
            && session
                .recipient
                .as_ref()
                .is_some_and(|r| r.worker_id == worker && r.expires_at.is_none_or(|t| t > now())),
        "native session does not permit this worker/channel/project"
    );
    Ok(session)
}

pub(crate) fn permit_tool(
    raw: &str,
    worker: &str,
    channel: &str,
    project: &str,
    name: &str,
    args: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let session = task_session(raw, worker, channel, project)?;
    let operation = native_operation(name)
        .ok_or_else(|| anyhow::anyhow!("tool is unavailable in native mode"))?;
    ensure!(
        session.operations.contains(&operation),
        "native tool is outside this task's capability grant"
    );
    if matches!(
        name,
        "native_edit_file" | "native_find_and_replace" | "native_suggest_edit"
    ) {
        let track = args
            .get("track")
            .map(|v| {
                v.as_str()
                    .ok_or_else(|| anyhow::anyhow!("native track must be a string"))
            })
            .transpose()?
            .unwrap_or("on");
        ensure!(
            matches!(track, "on" | "off" | "auto"),
            "invalid native tracking mode"
        );
        ensure!(
            track == "on" || session.operations.iter().any(|op| op == "direct_edit"),
            "direct editing requires its separate capability"
        );
    }
    match operation.as_str() {
        "ping" | "list_projects" => ensure!(
            !args.contains_key("project_id"),
            "native account reads do not take a project target"
        ),
        "create_project" => ensure!(
            project == "unbound" && session.projects.is_none() && !args.contains_key("project_id"),
            "project creation requires an unbound unrestricted account grant"
        ),
        _ => {
            let target = args
                .get("project_id")
                .and_then(|p| p.as_str())
                .ok_or_else(|| {
                    anyhow::anyhow!("native project call needs an explicit project_id")
                })?;
            ensure!(valid_id(target), "invalid native project target");
            if project == "unbound" {
                ensure!(
                    !native_tool_is_mutation(name)
                        && session
                            .projects
                            .as_ref()
                            .is_none_or(|ids| ids.iter().any(|id| id == target)),
                    "unbound native tasks may only read permitted projects; bind the host thread before writes"
                );
            } else {
                ensure!(
                    target == project,
                    "native project call differs from the host-confirmed task project"
                );
            }
        }
    }
    Ok(())
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
            project: Some(PROJECT.into()),
            channel: Some(CHANNEL.into()),
        };
        let r = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "dungeon3".into(),
            workspace: "snapy".into(),
            expires_at: None,
        };
        let s = serde_json::json!({"schema":2,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":"owner@example.invalid", "projects":[PROJECT], "operations":OPERATIONS});
        let b: serde_json::Value = serde_json::from_str(
            &bound_session(
                &s.to_string(),
                &r,
                Some(&r),
                &scope,
                CHANNEL,
                DEFAULT_ORIGIN,
                [],
            )
            .unwrap(),
        )
        .unwrap();
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
        let mut bound = b.clone();
        let raw = bound.to_string();
        assert!(task_session(&raw, &r.worker_id, CHANNEL, PROJECT).is_ok());
        for (worker, channel, project) in [
            (
                "worker-22222222-2222-4222-8222-222222222222",
                CHANNEL,
                PROJECT,
            ),
            (&r.worker_id, "COTHER", PROJECT),
            (&r.worker_id, CHANNEL, "444444444444444444444444"),
        ] {
            assert!(task_session(&raw, worker, channel, project).is_err());
        }
        let args = serde_json::json!({"project_id":PROJECT});
        assert!(permit_tool(
            &raw,
            &r.worker_id,
            CHANNEL,
            PROJECT,
            "native_read_file",
            args.as_object().unwrap()
        )
        .is_ok());
        assert!(permit_tool(
            &raw,
            &r.worker_id,
            CHANNEL,
            PROJECT,
            "push",
            args.as_object().unwrap()
        )
        .is_err());
        bound["expires_at"] = 1.into();
        assert!(permit_tool(
            &bound.to_string(),
            &r.worker_id,
            CHANNEL,
            PROJECT,
            "native_read_file",
            args.as_object().unwrap()
        )
        .is_err());
        let mut timed = r;
        timed.expires_at = Some(now() + 60);
        let b: serde_json::Value = serde_json::from_str(
            &bound_session(
                &s.to_string(),
                &timed,
                Some(&timed),
                &scope,
                CHANNEL,
                DEFAULT_ORIGIN,
                [],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(b["expires_at"], timed.expires_at.unwrap());
    }
    #[test]
    fn account_session_retains_saved_and_configured_legacy_limits() {
        let scope = Scope {
            expected_email: "owner@example.invalid".into(),
            project: None,
            channel: None,
        };
        let recipient = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "test".into(),
            workspace: "paper".into(),
            expires_at: None,
        };
        let raw = serde_json::json!({"schema":3,"cookie":"overleaf_session2=fixture", "account_id":"111111111111111111111111", "expected_email":scope.expected_email,"operations":OPERATIONS});
        let bound: serde_json::Value = serde_json::from_str(
            &bound_session(
                &raw.to_string(),
                &recipient,
                None,
                &scope,
                CHANNEL,
                DEFAULT_ORIGIN,
                [],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(bound.get("projects").is_none());
        assert_eq!(bound["recipient"]["worker_id"], recipient.worker_id);
        let restricted = Scope {
            project: Some(PROJECT.into()),
            channel: Some(CHANNEL.into()),
            ..scope.clone()
        };
        let mut legacy: serde_json::Value = serde_json::from_str(
            &bound_session(
                &raw.to_string(),
                &recipient,
                None,
                &restricted,
                CHANNEL,
                DEFAULT_ORIGIN,
                [],
            )
            .unwrap(),
        )
        .unwrap();
        let deadline = now() + 3600;
        legacy["schema"] = 2.into();
        legacy["expires_at"] = deadline.into();
        legacy["recipient"]["expires_at"] = deadline.into();
        let legacy = legacy.to_string();
        let renewed = renew_session(legacy.as_bytes(), &raw.to_string()).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&renewed).unwrap();
        assert_eq!(saved["schema"], 3);
        assert_eq!(saved["projects"], serde_json::json!([PROJECT]));
        assert_eq!(saved["channel"], CHANNEL);
        assert_eq!(saved["recipient"]["worker_id"], recipient.worker_id);
        assert_eq!(saved["recipient"]["expires_at"], deadline);
        assert_eq!(saved["expires_at"], deadline);
        assert!(bound_session(
            &renewed,
            &recipient,
            None,
            &scope,
            "COTHER",
            DEFAULT_ORIGIN,
            []
        )
        .is_err());
        let other = Recipient {
            worker_id: "worker-22222222-2222-4222-8222-222222222222".into(),
            ..recipient.clone()
        };
        assert!(
            bound_session(&renewed, &other, None, &scope, CHANNEL, DEFAULT_ORIGIN, []).is_err()
        );
        assert!(renew_session(
            legacy.as_bytes(),
            &raw.to_string().replace("owner@", "other@")
        )
        .is_err());
        assert!(
            validate_session(&raw.to_string().replace("\"schema\":3", "\"schema\":2")).is_err()
        );
    }
    #[test]
    fn schema4_upgrade_and_renewal_preserve_capabilities_origin_and_limits() {
        let recipient = Recipient {
            worker_id: "worker-11111111-1111-4111-8111-111111111111".into(),
            machine: "test".into(),
            workspace: "paper".into(),
            expires_at: None,
        };
        let legacy = serde_json::json!({"schema":3,"cookie":"overleaf_session2=fixture","account_id":"111111111111111111111111","expected_email":"owner@example.invalid","projects":[PROJECT],"channel":CHANNEL,"recipient":recipient,"operations":OPERATIONS,"legacy_workspaces":["/tmp/legacy-paper"]});
        let mut unbound_legacy = legacy.clone();
        unbound_legacy["schema"] = 2.into();
        unbound_legacy["channel"] = serde_json::Value::Null;
        unbound_legacy["recipient"] = serde_json::Value::Null;
        assert!(upgrade_operations(&unbound_legacy.to_string()).is_err());
        let upgraded = upgrade_operations(&legacy.to_string()).unwrap();
        let mut restored: serde_json::Value = serde_json::from_str(&upgraded).unwrap();
        assert_eq!(restored["schema"], 4);
        assert!(restored["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "direct_edit"));
        restored["schema"] = legacy["schema"].clone();
        restored["operations"] = legacy["operations"].clone();
        assert_eq!(restored, legacy);
        let renewed = renew_session(legacy.to_string().as_bytes(), &upgraded).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&renewed).unwrap();
        assert_eq!(saved["operations"], legacy["operations"]);
        assert_eq!(saved["projects"], legacy["projects"]);
        assert_eq!(saved["legacy_workspaces"], legacy["legacy_workspaces"]);
        let mut other: serde_json::Value = serde_json::from_str(&upgraded).unwrap();
        other["base_url"] = "https://ce.example.invalid".into();
        assert!(renew_session(legacy.to_string().as_bytes(), &other.to_string()).is_err());
        other["base_url"] = "http://127.0.0.1:3000/".into();
        let session = parse_session(&other.to_string()).unwrap();
        let (origin, projects) = (session.base_url, session.projects);
        assert_eq!(origin, "http://127.0.0.1:3000");
        assert_eq!(projects, Some(vec![PROJECT.into()]));
        other["operations"] = serde_json::json!(["read_file", "read_file"]);
        assert!(validate_session(&other.to_string()).is_err());
        other["operations"] = serde_json::json!(["unknown_operation"]);
        assert!(validate_session(&other.to_string()).is_err());
        other["operations"] = serde_json::json!([]);
        other["projects"] = serde_json::json!([]);
        assert!(validate_session(&other.to_string()).is_ok());
        let mut invalid_legacy = legacy.clone();
        invalid_legacy["operations"] = serde_json::json!(["read_file"]);
        assert!(validate_session(&invalid_legacy.to_string()).is_err());
    }
    #[test]
    fn unbound_tasks_retain_saved_read_limits_and_cannot_write_projects() {
        let worker = "worker-11111111-1111-4111-8111-111111111111";
        let raw = serde_json::json!({"schema":4,"cookie":"overleaf_session2=fixture","account_id":"111111111111111111111111","expected_email":"owner@example.invalid","projects":[PROJECT],"channel":CHANNEL,"recipient":{"worker_id":worker,"machine":"test","workspace":"paper"},"operations":native_operations()});
        let permit =
            |raw: &serde_json::Value, project: &str, tool: &str, args: serde_json::Value| {
                permit_tool(
                    &raw.to_string(),
                    worker,
                    CHANNEL,
                    project,
                    tool,
                    args.as_object().unwrap(),
                )
            };
        assert!(task_session(&raw.to_string(), worker, CHANNEL, "unbound").is_ok());
        assert!(permit(&raw, "unbound", "native_ping", serde_json::json!({})).is_ok());
        assert!(permit(
            &raw,
            "unbound",
            "native_list_projects",
            serde_json::json!({})
        )
        .is_ok());
        assert!(permit(
            &raw,
            "unbound",
            "native_read_file",
            serde_json::json!({"project_id":PROJECT})
        )
        .is_ok());
        assert!(permit(
            &raw,
            "unbound",
            "native_read_file",
            serde_json::json!({"project_id":"444444444444444444444444"})
        )
        .is_err());
        assert!(permit(
            &raw,
            "unbound",
            "native_reply_comment",
            serde_json::json!({"project_id":PROJECT})
        )
        .is_err());
        assert!(permit(
            &raw,
            "unbound",
            "native_create_project",
            serde_json::json!({})
        )
        .is_err());
        assert!(permit(
            &raw,
            PROJECT,
            "native_reply_comment",
            serde_json::json!({"project_id":PROJECT})
        )
        .is_ok());
        assert!(permit(
            &raw,
            PROJECT,
            "native_reply_comment",
            serde_json::json!({"project_id":"444444444444444444444444"})
        )
        .is_err());
        assert!(permit(
            &raw,
            PROJECT,
            "native_edit_file",
            serde_json::json!({"project_id":PROJECT,"track":"off"})
        )
        .is_err());
        let full: serde_json::Value =
            serde_json::from_str(&upgrade_operations(&raw.to_string()).unwrap()).unwrap();
        assert!(permit(
            &full,
            PROJECT,
            "native_edit_file",
            serde_json::json!({"project_id":PROJECT,"track":"off"})
        )
        .is_ok());
        let mut account = raw;
        account["projects"] = serde_json::Value::Null;
        assert!(permit(
            &account,
            "unbound",
            "native_create_project",
            serde_json::json!({})
        )
        .is_ok());
        assert!(permit(
            &account,
            "unbound",
            "native_read_file",
            serde_json::json!({"project_id":"444444444444444444444444"})
        )
        .is_ok());
        account["expires_at"] = 1.into();
        assert!(permit(&account, "unbound", "native_ping", serde_json::json!({})).is_err());
        assert!(permit(
            &account,
            "unbound",
            "native_list_projects",
            serde_json::json!({})
        )
        .is_err());
    }
}
