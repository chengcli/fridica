//! Owner-only verified session import, status, revocation and recipient binding.
use crate::core::secret::SecretStore;
use crate::secrets::NativeStore;
use anyhow::Result;
use clap::Subcommand;

const ENTRY: &str = "fridica/overleaf/native-session";

#[derive(Subcommand)]
pub enum Command {
    /// Describe the scoped sign-in flow without reading credentials or contacting Overleaf.
    Plan,
    /// Report configured saved native-session status without performing a network request.
    Status,
    /// Remove the dedicated store entry; already running workers require separate revocation.
    Revoke,
    /// Receive the owner helper's verified session over stdin and save to the OS store.
    /// Never pass a cookie in argv or invoke from an MCP worker.
    ImportVerifiedSession,
    /// Bind native forwarding to one actual worker and registered placement until revoked.
    BindRecipient {
        #[arg(long)]
        config: std::path::PathBuf,
        #[arg(long)]
        worker_id: String,
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
        Command::Plan => println!(
            "{}",
            serde_json::json!({
                "live_validated":false,"project":crate::config::overleaf::PROJECT,
                "operations":crate::config::overleaf::OPERATIONS,"entry":ENTRY,
                "login":"portable owner-login.mjs reuses upstream dedicated-profile login and saved session",
                "lifetime":"actual cookie/server expiry or owner revocation; optional explicitly timed grant",
                "recipient":"default deny; exact dungeon3:snapy worker and C0CL1NW0DGS job channel",
                "transport":"OS credential store and existing encrypted SSH stdin route",
                "platform":"owner helper needs Node >=20 and Chrome; listener needs this Fridica feature build",
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
                serde_json::json!({"tools_verified":false,"native":status,"live_validated":false,"login_helper":"owner-login.mjs"})
            );
        }
        Command::Revoke => {
            NativeStore.delete(ENTRY)?;
            println!("Native session entry removed. Running workers retain their copies: stop those sessions and invalidate the isolated Overleaf session to complete revocation.");
        }
        Command::ImportVerifiedSession => {
            use std::io::Read;
            let mut raw = String::new();
            std::io::stdin()
                .take(16385)
                .read_to_string(&mut raw)
                .map_err(|_| anyhow::anyhow!("cannot read verified native session"))?;
            crate::config::overleaf::validate_session(&raw)?;
            // Session renewal cannot silently bind a different account.
            if let Some(previous) = NativeStore.get(ENTRY)? {
                let old: serde_json::Value = serde_json::from_slice(&previous).map_err(|_| {
                    anyhow::anyhow!("invalid saved session; explicit revocation required")
                })?;
                let new: serde_json::Value = serde_json::from_str(&raw)?;
                anyhow::ensure!(
                    old["account_id"] == new["account_id"]
                        && old["expected_email"] == new["expected_email"],
                    "native account changed; explicit revocation required"
                );
            }
            NativeStore.set(ENTRY, raw.as_bytes())?;
            println!("Verified native session saved. No worker started or credential forwarded.");
        }
        Command::BindRecipient {
            config,
            worker_id,
            machine,
            workspace,
            expires_at,
        } => {
            validate_expiry(expires_at)?;
            bind_recipient(&config, worker_id, machine, workspace, expires_at)?;
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
    let mut document = source.parse::<DocumentMut>()?;
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
