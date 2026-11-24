//! Owner entry point for a separate native Overleaf authorization. No cookie import.
use crate::core::secret::SecretStore;
use crate::secrets::NativeStore;
use anyhow::Result;
use clap::Subcommand;

const ENTRY: &str = "fridica/overleaf/native-session";
mod managed;

#[derive(Subcommand)]
pub enum Command {
    /// Describe the scoped sign-in flow without reading credentials or contacting Overleaf.
    Plan,
    /// Report configured saved native-session status without performing a network request.
    Status,
    /// Remove the dedicated store entry; already running workers require separate revocation.
    Revoke,
    /// Open one dedicated human-controlled Chrome login on macOS.
    SignIn {
        /// Expected email of the existing Overleaf account; verified against server metadata.
        #[arg(long)]
        expected_email: String,
        /// Optional Unix timestamp, only for an explicitly timed owner grant.
        #[arg(long)]
        expires_at: Option<u64>,
    },
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
                "live_enabled":false,"upstream":"@netique/overleaf-mcp@0.4.0","upstream_source":"cd248f856ba63af449eb58cb0eb0af04fd933ac0","gated_operations":["add_comment","find_and_replace"],"gate_reason":"upstream applyOtUpdate reconnect replay","project":"6b0483a71b304b7fd2141775","account":"existing happysky19 Overleaf account; bind verified server account ID at sign-in",
                "operations":crate::config::overleaf::OPERATIONS,"entry":ENTRY,"lease_seconds":null,
                "lifetime":"reuse saved session until actual cookie/server expiry or owner revocation; --expires-at only for an explicitly timed grant",
                "adapter":"chrome-cdp-pipe", "platform_supported":cfg!(target_os = "macos"),
                "mechanism":"fresh temporary Chrome profile on macOS; anonymous CDP pipes; one human login window; no existing profile or cookie import",
                "browser":"ordinary human-controlled Overleaf login, site resources, dashboard redirects and human-selected SSO; no browser URL firewall or SSO-origin setup; no page contents or provider cookies collected",
                "capture":"only fixed-Overleaf-URL cookie polling; no request events, Fetch interception, DOM inspection or page scripts; fixed-project verification retries pending login within five minutes",
                "identity":"--expected-email verifies the existing account and enrolls its server ID during the same login",
                "recipient":"default deny; exact dungeon3:snapy worker plus C0CL1NW0DGS job channel; optional explicit expiry",
                "transport":"existing encrypted SSH stdin framing to assigned worker; session supplied to the unmodified AGPL process through a sealed memory store",
                "activation":"install this owner command and the pinned worker adapter; bind one recipient; reload owner configuration",
                "revocation":"delete entry, stop identified native-session workers, invalidate isolated Overleaf session; deletion alone cannot revoke running copies",
                "limitation":"account bearer cookie; project/method/recipient gates are application authorization, not cryptographic project scoping or server-issued token permissions"
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
                serde_json::json!({"tools_verified":false,"native":status,"live_validated":false,"sign_in_adapter_available":cfg!(target_os = "macos")})
            );
        }
        Command::Revoke => {
            NativeStore.delete(ENTRY)?;
            println!("Native session entry removed. Running workers retain their copies: stop those sessions and invalidate the isolated Overleaf session to complete revocation.");
        }
        Command::SignIn {
            expected_email,
            expires_at,
        } => {
            validate_expiry(expires_at)?;
            tokio::runtime::Handle::current()
                .block_on(managed::sign_in(&expected_email, expires_at))?;
            println!("Native Overleaf session saved for SNAP V3. Native anchored comments and tracked edits remain gated pending upstream no-replay support. Reused until cookie/server expiry, any explicit deadline, or owner revocation. No worker was started or credential forwarded.");
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
