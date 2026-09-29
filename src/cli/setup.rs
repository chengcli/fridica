//! Owner setup commands. Discovery reads Slack only when explicitly requested;
//! file changes follow successful selection and never start a daemon.
use crate::{
    config::{
        setup::{self, Draft, Identity},
        LoadContext,
    },
    slack::discovery::{self, Api, Discovery},
};
use anyhow::{bail, Result};
use clap::Args;
use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
};

#[derive(Args)]
pub struct Configure {
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long, conflicts_with_all=["owner_id","workspace_id","channels"])]
    pub detect: bool,
    #[arg(long = "channel-name", requires = "detect")]
    pub names: Vec<String>,
    #[arg(long)]
    pub owner_id: Option<String>,
    #[arg(long)]
    pub workspace_id: Option<String>,
    #[arg(long = "channel-id")]
    pub channels: Vec<String>,
}
/// Injectable selection for CLI and local fixtures; commit only after selection.
pub async fn detected(
    draft: Draft,
    api: &dyn Api,
    select: impl FnOnce(&Discovery) -> Result<Vec<String>>,
) -> Result<()> {
    let result = discovery::discover(api).await?;
    let channels = select(&result)?;
    if channels.is_empty()
        || channels
            .iter()
            .any(|id| !result.channels.iter().any(|c| &c.id == id))
    {
        bail!("selection is not a discovered joined channel");
    }
    draft.apply(Identity {
        owner: Some(result.owner),
        workspace: Some(result.workspace),
        channels,
    })?;
    Ok(())
}
impl Configure {
    pub async fn run(self, context: &LoadContext) -> Result<()> {
        let path = setup::path(self.config.as_deref(), context)?;
        let draft = Draft::open(path.clone())?;
        if self.detect {
            let variable = draft.token_variable()?;
            let token = std::env::var(&variable).map_err(|_| {
                anyhow::anyhow!("set {variable} to your Slack user token before detection")
            })?;
            let client = discovery::Web::new(&token)?;
            detected(draft, &client, |found| {
                println!(
                    "Detected owner {} in workspace {}.",
                    found.owner, found.workspace
                );
                for warning in &found.warnings {
                    eprintln!("{warning}");
                }
                select(found, &self.names)
            })
            .await?;
        } else {
            draft.apply(Identity {
                owner: self.owner_id,
                workspace: self.workspace_id,
                channels: self.channels,
            })?;
        }
        println!("Updated {}. Complete the workspaces and inventories, then use this executable with the same --config path for check-config and start --check-ready. Existing daemons require a restart to use these identity changes.", path.display());
        Ok(())
    }
}

/// No implicit all-channel selection when stdin is not a terminal.
pub fn select(found: &Discovery, names: &[String]) -> Result<Vec<String>> {
    let channels = &found.channels;
    if !names.is_empty() {
        return discovery::select_names(channels, names);
    }
    if channels.is_empty() {
        bail!("No joined channels found; check membership and channel read scopes");
    }
    if !io::stdin().is_terminal() {
        bail!("Channel selection needs a terminal; use --channel-name NAME (repeat for multiple channels)");
    }
    for (index, channel) in channels.iter().enumerate() {
        println!(
            "  {}. #{} ({})",
            index + 1,
            channel.name.escape_debug(),
            channel.id
        );
    }
    print!("Select channel numbers separated by commas (blank cancels): ");
    io::stdout().flush()?;
    // Bound terminal input without echoing invalid content in errors.
    use std::io::BufRead;
    let input = io::stdin();
    let mut input = input.lock();
    let mut bytes = Vec::new();
    let mut bounded = std::io::Read::take(&mut input, 4097);
    bounded.read_until(b'\n', &mut bytes)?;
    if bytes.len() > 4096 {
        bail!("Invalid channel selection; configuration unchanged");
    }
    let answer = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::anyhow!("Invalid channel selection; configuration unchanged"))?;
    discovery::select_numbers(channels, answer)
}
