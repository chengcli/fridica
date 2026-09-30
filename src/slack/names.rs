//! Readable Slack names for owner-facing output and input. Thread IDs stay
//! `WORKSPACE:CHANNEL:TS` in state; owners see and may type `#channel:TS`.
//! Names are recorded by the startup check and never grant scope: only
//! configured channels have names here.
use crate::{
    config::schema::Slack,
    core::ids::{ChannelId, SlackTs, ThreadId, WorkspaceId},
};
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Names {
    pub workspace: String,
    pub workspace_name: String,
    /// Configured channel IDs; only these resolve.
    pub configured: Vec<String>,
    /// Recorded names of configured channels.
    pub channels: BTreeMap<String, String>,
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
        })
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
}
