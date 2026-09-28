//! CLI controls are clients of the socket API, never database writers.
use super::client::Client;
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use serde_json::{json, Value};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
};
#[derive(Args)]
pub struct Connection {
    #[arg(long, conflicts_with = "socket")]
    config: Option<PathBuf>,
    /// Connect directly to an existing control socket.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Owner-only file containing a capability secret;
    /// never passed in argv.
    #[arg(long)]
    capability_file: Option<PathBuf>,
}
impl Connection {
    fn client(self) -> Result<Client> {
        let socket = match self.socket {
            Some(path) => path,
            None => {
                let context = crate::config::LoadContext::current()?;
                let path = self
                    .config
                    .unwrap_or_else(|| context.home.join(".config/fridica/config.toml"));
                crate::config::load(&path, &context)?.state.control_socket
            }
        };
        let token = self
            .capability_file
            .map(|path| -> Result<String> {
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(
                        (rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::NONBLOCK).bits() as i32,
                    )
                    .open(path)
                    .context("cannot open capability file")?;
                let m = file.metadata()?;
                if !m.is_file()
                    || m.uid() != users::get_current_uid()
                    || m.mode() & 0o077 != 0
                    || m.nlink() != 1
                {
                    bail!("capability file must be a private regular file owned by this user");
                }
                let mut token = String::new();
                file.take(66)
                    .read_to_string(&mut token)
                    .context("invalid capability file")?;
                let token = token.trim_end_matches('\n');
                if m.len() > 65
                    || token.len() != 64
                    || !token.bytes().all(|c| c.is_ascii_hexdigit())
                {
                    bail!("invalid capability file");
                }
                Ok(token.to_string())
            })
            .transpose()?;
        Ok(Client::new(&socket, token)?)
    }
}
#[derive(Subcommand)]
pub enum Commands {
    /// Show daemon status.
    Status {
        #[command(flatten)]
        connection: Connection,
    },
    /// List threads, show one, or apply a control.
    Threads {
        id: Option<String>,
        #[arg(value_parser=["pause","resume","close","archive","restore","clean"], requires="id")]
        action: Option<String>,
        #[command(flatten)]
        connection: Connection,
    },
    /// List workers or interrupt/stop one.
    Workers {
        id: Option<String>,
        #[arg(value_parser=["interrupt","stop"], requires="id")]
        action: Option<String>,
        #[command(flatten)]
        connection: Connection,
    },
    /// List pending approvals or decide one.
    Approvals {
        id: Option<String>,
        #[arg(value_parser=["once","session","deny"], requires="id")]
        decision: Option<String>,
        #[command(flatten)]
        connection: Connection,
    },
    /// Show machines and their current use.
    Machines {
        #[command(flatten)]
        connection: Connection,
    },
    /// List failed/ambiguous posts, or explicitly retry one.
    Outbox {
        id: Option<i64>,
        #[command(flatten)]
        connection: Connection,
    },
    /// Queue an owner instruction. Reuse the client ID after an uncertain response.
    Instruct {
        id: String,
        #[arg(long)]
        text: String,
        #[arg(long)]
        client_id: String,
        #[command(flatten)]
        connection: Connection,
    },
}
fn segment(value: &str) -> Result<&str> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.:".contains(&c))
    {
        bail!("invalid control identifier");
    }
    Ok(value)
}
impl Commands {
    pub async fn run(self) -> Result<Value> {
        let (connection, method, target, body) = match self {
            Self::Status { connection } => (connection, "GET", "/status".into(), None),
            Self::Machines { connection } => (connection, "GET", "/machines".into(), None),
            Self::Threads {
                id,
                action,
                connection,
            } => match (id, action) {
                (Some(id), Some(action)) => (
                    connection,
                    "POST",
                    format!("/threads/{}/{}", segment(&id)?, action),
                    Some(json!({})),
                ),
                (Some(id), None) => (
                    connection,
                    "GET",
                    format!("/threads/{}", segment(&id)?),
                    None,
                ),
                (None, None) => (connection, "GET", "/threads".into(), None),
                _ => bail!("thread action requires an identifier"),
            },
            Self::Workers {
                id,
                action,
                connection,
            } => match (id, action) {
                (Some(id), Some(action)) => (
                    connection,
                    "POST",
                    format!("/workers/{}/{}", segment(&id)?, action),
                    Some(json!({})),
                ),
                (_, None) => (connection, "GET", "/workers".into(), None),
                _ => bail!("worker action requires an identifier"),
            },
            Self::Approvals {
                id,
                decision,
                connection,
            } => match (id, decision) {
                (Some(id), Some(decision)) => (
                    connection,
                    "POST",
                    format!("/approvals/{}", segment(&id)?),
                    Some(json!({"decision":decision})),
                ),
                (_, None) => (connection, "GET", "/approvals".into(), None),
                _ => bail!("approval decision requires an identifier"),
            },
            Self::Outbox { id, connection } => match id {
                Some(id) if id > 0 => (
                    connection,
                    "POST",
                    format!("/outbox/{id}/retry"),
                    Some(json!({})),
                ),
                None => (connection, "GET", "/outbox".into(), None),
                _ => bail!("invalid outbox identifier"),
            },
            Self::Instruct {
                id,
                text,
                client_id,
                connection,
            } => (
                connection,
                "POST",
                format!("/threads/{}/instruct", segment(&id)?),
                Some(json!({"text":text,"client_id":client_id})),
            ),
        };
        Ok(connection.client()?.request(method, &target, body).await?)
    }
}
