//! Readable Slack names for owner-facing output and input. Thread IDs stay
//! `WORKSPACE:CHANNEL:TS` in state; owners see and may type `#channel:TS`.
//! Names are recorded by the startup check and never grant scope: only
//! configured channels have names here. Member names are looked up through
//! `users.info` when first needed and kept for later output.
use crate::{
    config::schema::Slack,
    core::ids::{ChannelId, SlackTs, ThreadId, WorkspaceId},
};
use fridica_slack::BoxFuture;
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;

/// Recorded member names are bounded; past this, a new name replaces none.
const USER_LIMIT: usize = 5000;

#[derive(Clone, Debug, Default)]
pub struct Names {
    pub workspace: String,
    pub workspace_name: String,
    /// Configured channel IDs; only these resolve.
    pub configured: Vec<String>,
    /// Recorded names of configured channels.
    pub channels: BTreeMap<String, String>,
    /// Recorded names of workspace members, by user ID.
    pub users: BTreeMap<String, String>,
}
/// Looks up a member's readable name; `None` when it cannot be had.
pub trait UserNames: Send + Sync {
    fn user_name<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Option<String>>;
}
impl UserNames for super::web::SlackClient {
    fn user_name<'a>(&'a self, user: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { (**self).user_name(user).await.ok().flatten() })
    }
}
/// Keep a member's name for later output.
pub fn record_user(c: &Connection, user: &str, name: &str) -> rusqlite::Result<()> {
    let mut users: BTreeMap<String, String> = meta(c, "slack_user_names")?
        .and_then(|v| serde_json::from_str(&v).ok())
        .unwrap_or_default();
    if users.len() >= USER_LIMIT && !users.contains_key(user) {
        return Ok(());
    }
    users.insert(user.into(), name.into());
    c.execute(
        "INSERT INTO meta VALUES('slack_user_names',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [serde_json::to_string(&users).unwrap_or_default()],
    )?;
    Ok(())
}

fn meta(c: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    c.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()
}

impl Names {
    pub fn load(c: &Connection, slack: &Slack) -> rusqlite::Result<Self> {
        Ok(Self {
            workspace: slack.workspace.clone(),
            configured: slack.channels.clone(),
            workspace_name: meta(c, "slack_workspace_name")?.unwrap_or_default(),
            channels: meta(c, "slack_channel_names")?
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default(),
            users: meta(c, "slack_user_names")?
                .and_then(|v| serde_json::from_str(&v).ok())
                .unwrap_or_default(),
        })
    }
    /// `@name`, or the ID when no name was recorded.
    pub fn user(&self, id: &str) -> String {
        self.users
            .get(id)
            .map_or_else(|| id.to_string(), |n| format!("@{n}"))
    }
    /// `#name`, or the ID when no name was recorded.
    pub fn channel(&self, id: &str) -> String {
        self.channels
            .get(id)
            .map_or_else(|| id.to_string(), |n| format!("#{n}"))
    }
    /// A configured channel's ID from its ID, `name` or `#name`.
    pub fn channel_id(&self, reference: &str) -> Option<String> {
        let wanted = reference.trim_start_matches('#');
        self.configured
            .iter()
            .find(|id| *id == wanted || self.channels.get(*id).is_some_and(|n| n == wanted))
            .cloned()
    }
    /// `#channel:TS` for a thread ID, or the ID when it is not in this workspace.
    pub fn thread(&self, id: &str) -> String {
        match id.parse::<ThreadId>() {
            Ok(t) if t.workspace.0 == self.workspace => {
                format!("{}:{}", self.channel(&t.channel.0), t.root_ts)
            }
            _ => id.to_string(),
        }
    }
    /// The thread ID for `#channel:TS`, `channel:TS` or `CHANNEL_ID:TS`;
    /// full thread IDs pass through unchanged.
    pub fn resolve(&self, reference: &str) -> Option<String> {
        if let Ok(id) = reference.parse::<ThreadId>() {
            return Some(id.to_string());
        }
        let (channel, ts) = reference.split_once(':')?;
        (!ts.is_empty()).then_some(())?;
        let id = ThreadId {
            workspace: WorkspaceId(self.workspace.clone()),
            channel: ChannelId(self.channel_id(channel)?),
            root_ts: SlackTs(ts.into()),
        };
        Some(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names() -> Names {
        Names {
            workspace: "T1".into(),
            workspace_name: "scix".into(),
            configured: vec!["C1".into(), "C2".into()],
            channels: [("C1".to_string(), "ai-human-plume".to_string())].into(),
            users: BTreeMap::new(),
        }
    }
    #[test]
    fn threads_read_and_resolve_by_channel_name() {
        let n = names();
        assert_eq!(n.thread("T1:C1:100.1"), "#ai-human-plume:100.1");
        assert_eq!(n.thread("T1:C9:100.1"), "C9:100.1");
        assert_eq!(n.thread("T2:C1:100.1"), "T2:C1:100.1");
        for reference in [
            "#ai-human-plume:100.1",
            "ai-human-plume:100.1",
            "C1:100.1",
            "T1:C1:100.1",
        ] {
            assert_eq!(n.resolve(reference).as_deref(), Some("T1:C1:100.1"));
        }
        // A configured channel without a recorded name still resolves by ID.
        assert_eq!(n.resolve("C2:100.1").as_deref(), Some("T1:C2:100.1"));
        assert_eq!(n.resolve("#unknown:100.1"), None);
        assert_eq!(n.resolve("CUNLISTED:100.1"), None);
        assert_eq!(n.resolve("ai-human-plume:"), None);
        assert_eq!(n.resolve("ai-human-plume"), None);
    }
    #[test]
    fn members_read_by_recorded_name() {
        let mut n = names();
        assert_eq!(n.user("U7"), "U7");
        n.users.insert("U7".into(), "Ada".into());
        assert_eq!(n.user("U7"), "@Ada");
    }
}
