use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

macro_rules! string_id {
    ($($name:ident),+ $(,)?) => {$(
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { self.0.fmt(f) }
        }
    )+};
}
string_id!(
    WorkspaceId,
    ChannelId,
    SlackTs,
    EventId,
    WorkerId,
    JobId,
    BackendSessionId,
    ObligationId,
    WorkItemId,
    ExchangeId
);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadId {
    pub workspace: WorkspaceId,
    pub channel: ChannelId,
    pub root_ts: SlackTs,
}
impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.workspace, self.channel, self.root_ts)
    }
}
impl FromStr for ThreadId {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<_> = s.split(':').collect();
        if parts.len() != 3 || parts.iter().any(|s| s.is_empty()) {
            return Err("expected workspace:channel:root_ts");
        }
        Ok(Self {
            workspace: WorkspaceId(parts[0].into()),
            channel: ChannelId(parts[1].into()),
            root_ts: SlackTs(parts[2].into()),
        })
    }
}
