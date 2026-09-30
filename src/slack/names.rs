//! Readable Slack names for owner-facing output and input. Thread IDs stay
//! `WORKSPACE:CHANNEL:TS` in state; owners see and may type `#channel:TS`.
//! Names are recorded by the startup check and never grant scope: only
//! configured channels have names here.
use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct Names {
    pub workspace: String,
    pub workspace_name: String,
    pub channels: BTreeMap<String, String>,
}

fn meta(c: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    c.query_row("SELECT value FROM meta WHERE key=?", [key], |r| r.get(0))
        .optional()
}

impl Names {
    pub fn load(c: &Connection, workspace: &str) -> rusqlite::Result<Self> {
        Ok(Self {
            workspace: workspace.into(),
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
        self.channels
            .iter()
            .find(|(id, name)| *id == wanted || *name == wanted)
            .map(|(id, _)| id.clone())
    }
    /// `#channel:TS` for a thread ID, or the ID when it is not in this workspace.
    pub fn thread(&self, id: &str) -> String {
        match id.splitn(3, ':').collect::<Vec<_>>()[..] {
            [workspace, channel, ts] if workspace == self.workspace => {
                format!("{}:{ts}", self.channel(channel))
            }
            _ => id.to_string(),
        }
    }
    /// The thread ID for `#channel:TS`, `channel:TS` or `CHANNEL_ID:TS`;
    /// full thread IDs pass through unchanged.
    pub fn resolve(&self, reference: &str) -> Option<String> {
        match reference.splitn(3, ':').collect::<Vec<_>>()[..] {
            [_, _, _] => Some(reference.to_string()),
            [channel, ts] => Some(format!(
                "{}:{}:{ts}",
                self.workspace,
                self.channel_id(channel)?
            )),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names() -> Names {
        Names {
            workspace: "T1".into(),
            workspace_name: "scix".into(),
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
        assert_eq!(n.resolve("#unknown:100.1"), None);
        assert_eq!(n.resolve("ai-human-plume"), None);
    }
}
