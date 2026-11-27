//! Owner-only verified session import, status, revocation and recipient binding.
use crate::core::secret::SecretStore;
use crate::secrets::NativeStore;
use anyhow::Result;
use clap::Subcommand;

mod login;

const ENTRY: &str = "fridica/overleaf/native-session";

#[derive(Subcommand)]
pub enum Command {
    /// Reuse the saved Overleaf login, or sign in with an isolated browser when expired.
    Login(login::Options),
    /// Describe the scoped sign-in flow without reading credentials or contacting Overleaf.
    Plan,
    /// Allow the full native tool set on the saved login without signing in again.
    /// Keeps all existing account, origin, channel, project, recipient and expiry limits.
    Grant {
        #[arg(long, required = true)]
        all: bool,
    },
    /// Report configured saved native-session status without performing a network request.
    Status,
    /// Remove the dedicated store entry; already running workers require separate revocation.
    Revoke,
    /// Receive a verified session over stdin and save to the OS store.
    /// Never pass a cookie in argv or invoke from an MCP worker.
    ImportVerifiedSession {
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
    /// Bind native forwarding to one actual worker and registered placement until revoked.
    BindRecipient {
        #[arg(long)]
        config: std::path::PathBuf,
        #[arg(long)]
        worker_id: String,
        /// Exact job channel. May already be configured by login.
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        machine: String,
        #[arg(long)]
        workspace: String,
        /// Optional Unix timestamp, only for an explicitly timed owner grant.
        #[arg(long)]
        expires_at: Option<u64>,
    },
}

pub fn run(command: Command) -> Result<()> {
    match command {
        Command::Login(options) => login::run(options)?,
        Command::Grant { all } => {
            anyhow::ensure!(all, "specify --all to authorize the full native tool set");
            let previous = NativeStore
                .get(ENTRY)?
                .ok_or_else(|| anyhow::anyhow!("no saved Overleaf login"))?;
            let raw = std::str::from_utf8(&previous)
                .map_err(|_| anyhow::anyhow!("invalid native session encoding"))?;
            let updated = crate::config::overleaf::upgrade_operations(raw)?;
            NativeStore.set(ENTRY, updated.as_bytes())?;
            println!("Full native tools authorized on the existing login. Account, origin, project, channel and expiry limits retained. New admitted tasks receive the updated grant.");
        }
        Command::Plan => println!(
            "{}",
            serde_json::json!({
                "live_validated":false,"scope":"one verified account; optional project/channel/recipient restrictions",
                "operations":fridica_agent::overleaf::native_operations(),"entry":ENTRY,
                "login":"fridica overleaf-auth login validates and reuses the saved session; Rust opens an isolated browser only for missing or expired login",
                "lifetime":"actual cookie/server expiry or owner revocation; optional explicitly timed grant",
                "recipient":"automatic for admitted eligible workers in allowed channels; legacy restrictions retained",
                "transport":"OS credential store and existing encrypted SSH stdin route",
                "platform":"built-in Rust login; Chrome or Chromium only when a fresh sign-in is needed",
                "revocation":"delete entry, stop holder workers and invalidate the dedicated Overleaf session"
            })
        ),
        Command::Status => {
            let from_env = std::env::var("OVERLEAF_NATIVE_SESSION").ok();
            let raw = match from_env {
                Some(value) if !value.trim().is_empty() => Some(value),
                _ => NativeStore
                    .get(ENTRY)?
                    .map(String::from_utf8)
                    .transpose()
                    .map_err(|_| anyhow::anyhow!("invalid native session encoding"))?,
            };
            let status = match raw {
                Some(value) => match crate::config::overleaf::validate_session(&value) {
                    Ok(()) => "configured_unverified",
                    Err(_) => "invalid_or_expired",
                },
                None => "not_configured",
            };
            println!(
                "{}",
                serde_json::json!({"tools_verified":false,"native":status,"live_validated":false,"login_helper":"rust"})
            );
        }
        Command::Revoke => {
            NativeStore.delete(ENTRY)?;
            println!("Native session entry removed. Running workers retain their copies: stop those sessions and invalidate the isolated Overleaf session to complete revocation.");
        }
        Command::ImportVerifiedSession { config } => {
            use std::io::Read;
            let mut raw = String::new();
            std::io::stdin()
                .take(16385)
                .read_to_string(&mut raw)
                .map_err(|_| anyhow::anyhow!("cannot read verified native session"))?;
            crate::config::overleaf::validate_session(&raw)?;
            // Session renewal must preserve both identity and old restrictive grants.
            if let Some(previous) = NativeStore.get(ENTRY)? {
                raw = crate::config::overleaf::renew_session(&previous, &raw)?;
            }
            if let Some(path) = config {
                let source = crate::config::editor::read(&path)?;
                let document = source.parse::<toml_edit::DocumentMut>()?;
                let settings: crate::config::overleaf::Settings =
                    crate::config::loader::decode(document.get("overleaf"))?;
                let scope = settings
                    .native_scope
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("configure native login scope first"))?;
                crate::config::overleaf::validate_session_scope(&raw, scope)?;
            }
            NativeStore.set(ENTRY, raw.as_bytes())?;
            println!("Verified native session saved. No worker started or credential forwarded.");
        }
        Command::BindRecipient {
            config,
            worker_id,
            channel,
            machine,
            workspace,
            expires_at,
        } => {
            validate_expiry(expires_at)?;
            bind_recipient(&config, worker_id, channel, machine, workspace, expires_at)?;
            println!("Native recipient bound in owner configuration until revoked or an explicit deadline. Load this configuration during activation; no credential was read or forwarded.");
        }
    }
    Ok(())
}

fn validate_expiry(expires_at: Option<u64>) -> Result<()> {
    anyhow::ensure!(
        expires_at.is_none_or(|t| t > crate::config::overleaf::now() && t <= i64::MAX as u64),
        "explicit native expiry must be a future Unix timestamp"
    );
    Ok(())
}

fn bind_recipient(
    path: &std::path::Path,
    worker_id: String,
    channel: Option<String>,
    machine: String,
    workspace: String,
    expires_at: Option<u64>,
) -> Result<()> {
    use crate::config::{editor, loader, overleaf::Recipient, LoadContext};
    use toml_edit::{value, DocumentMut, Item, Table};
    let context = LoadContext::current()?;
    let source = editor::read(path)?;
    let config = loader::parse(&source, path, &context)?;
    anyhow::ensure!(
        config.overleaf.enabled,
        "enable the ordinary Overleaf integration before binding a recipient"
    );
    let recipient = Recipient {
        worker_id,
        machine,
        workspace,
        expires_at,
    };
    recipient.validate()?;
    let mut scope = config.overleaf.native_scope.clone().ok_or_else(|| {
        anyhow::anyhow!("run overleaf-auth login with the expected account first")
    })?;
    if let Some(channel) = channel {
        scope.channel = Some(channel);
    }
    scope.validate()?;
    let channel = scope
        .channel
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("an explicit --channel is required for forwarding"))?;
    anyhow::ensure!(
        config.slack.channels.iter().any(|c| c == channel),
        "native channel must be in the owner Slack allowlist"
    );
    let mut document = source.parse::<DocumentMut>()?;
    document["overleaf"]["native_scope"]["channel"] = value(channel);
    let mut table = Table::new();
    table["worker_id"] = value(recipient.worker_id);
    table["machine"] = value(recipient.machine);
    table["workspace"] = value(recipient.workspace);
    if let Some(deadline) = recipient.expires_at {
        table["expires_at"] = value(deadline as i64);
    }
    document["overleaf"]["native_recipient"] = Item::Table(table);
    let updated = document.to_string();
    loader::parse(&updated, &config.path, &context)?;
    editor::replace(&config.path, &config.fingerprint, &updated)
}
