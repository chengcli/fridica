use anyhow::Result;
use clap::{Parser, Subcommand};
use fridica::{
    core::time::{Clock, SystemClock},
    report,
    store::{migration, Store},
};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Experimental v0.4 configuration, migration and offline report tools. The Python launcher remains the production daemon."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Validate configuration and resolved placement policies without starting adapters.
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// Migrate a stopped daemon's database and configuration, with backups.
    Migrate {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long, conflicts_with = "rollback")]
        dry_run: bool,
        #[arg(long)]
        rollback: bool,
    },
    /// Generate and atomically export a report from an offline v6 database.
    Report {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        date: chrono::NaiveDate,
        #[arg(long)]
        timezone: chrono_tz::Tz,
        #[arg(long)]
        directory: PathBuf,
    },
    /// Show the exact baseline assets used by this development build.
    Assets {
        #[arg(long)]
        list: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::CheckConfig { config } => {
            let config = fridica::config::load(&config, &fridica::config::LoadContext::current()?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "fingerprint": config.fingerprint,
                    "machines": config.machines.names(),
                    "default_machine": config.parent.default_machine,
                    "attention": config.attention,
                }))?
            );
        }
        Command::Migrate {
            database,
            config,
            dry_run,
            rollback,
        } => {
            if rollback {
                migration::rollback(&database, &config)?;
                println!("Restored migration backups.");
            } else {
                let plan = if dry_run {
                    migration::dry_run(&database, &config)?
                } else {
                    migration::migrate(&database, &config, SystemClock.now())?
                };
                println!("{}", serde_json::to_string_pretty(&plan)?);
            }
        }
        Command::Report {
            database,
            channel,
            date,
            timezone,
            directory,
        } => {
            let store = Store::open(database).await?;
            let data = report::generate(&store, channel, date, timezone, SystemClock.now()).await?;
            report::export_pending(&store, directory).await?;
            println!("{}", serde_json::to_string_pretty(&data)?);
        }
        Command::Assets { list: _ } => {
            use sha2::{Digest, Sha256};
            let assets: [(&str, &[u8]); 5] = [
                (
                    "contract.md",
                    include_bytes!("../fridica/parent/contract.md"),
                ),
                ("repos.toml", include_bytes!("../fridica/parent/repos.toml")),
                (
                    "template.toml",
                    include_bytes!("../fridica/config/template.toml"),
                ),
                ("manifest.yaml", include_bytes!("../../slack/manifest.yaml")),
                (
                    "dashboard/index.html",
                    include_bytes!("../fridica/dashboard/static/index.html"),
                ),
            ];
            for (name, data) in assets {
                println!("{:x}  {name}", Sha256::digest(data));
            }
        }
    }
    Ok(())
}
