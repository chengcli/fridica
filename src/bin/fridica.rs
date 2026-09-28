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
    about = "Experimental v0.4 observer, controls, migration and offline report tools. The Python launcher remains the production daemon."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create an experimental starter configuration, contract and Slack manifest.
    Init {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Fill Slack identity and channel settings before the configuration is complete.
    Configure(fridica::cli::setup::Configure),
    #[command(flatten)]
    Control(fridica::control::cli::Commands),
    /// Experimental Slack observer with owner-authenticated local controls.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Required for service startup until the remaining active-launch gates pass.
        #[arg(long)]
        observe_only: bool,
        /// Read-only startup preparation; no Slack credentials or state required.
        #[arg(long, conflicts_with = "observe_only")]
        check_ready: bool,
        #[arg(long, requires = "check_ready", value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout: Option<u64>,
    },
    /// Validate configuration and resolved placement policies without starting adapters.
    CheckConfig {
        #[arg(long)]
        config: PathBuf,
    },
    /// Test worker isolation on one explicit local/SSH target without a backend.
    DoctorIsolation {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        machine: String,
        #[arg(long)]
        workspace: String,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout: u64,
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
    /// Show the embedded assets used by this development build.
    Assets {
        #[arg(long)]
        list: bool,
    },
}
#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init { config } => {
            let context = fridica::config::LoadContext::current()?;
            let path = fridica::config::setup::path(config.as_deref(), &context)?;
            fridica::config::setup::init(&path)?;
            println!(
                "Created {}. Agent rules are in {}.",
                path.display(),
                path.parent().unwrap().join("contract.md").display()
            );
            println!("Next: fridica configure --detect, then complete workspaces and inventories, run fridica check-config and fridica start --check-ready. Active CLI startup remains gated.");
        }
        Command::Configure(options) => {
            if let Err(error) = options.run(&fridica::config::LoadContext::current()?).await {
                eprintln!("fridica: {error}");
                std::process::exit(2);
            }
        }
        Command::Start {
            config,
            observe_only,
            check_ready,
            timeout,
        } => {
            if !observe_only && !check_ready {
                anyhow::bail!("experimental Rust start requires --observe-only; active launch awaits compatibility, replay, packaging and deployment gates; use --check-ready for startup preparation");
            }
            let context = fridica::config::LoadContext::current()?;
            let path = config.unwrap_or_else(|| context.home.join(".config/fridica/config.toml"));
            let config = fridica::config::load(&path, &context)?;
            if check_ready {
                let report = with_shutdown(|stop| async move {
                    fridica::doctor::readiness::check(
                        &config,
                        &context,
                        std::env::vars_os().collect(),
                        std::time::Duration::from_secs(timeout.unwrap_or(30)),
                        stop,
                    )
                    .await
                })
                .await?;
                println!("{}", serde_json::to_string_pretty(&report)?);
                if !report.startup_checks_passed {
                    anyhow::bail!("startup readiness checks did not pass");
                }
            } else {
                let credentials =
                    fridica::daemon::Credentials::read(&config, |name| std::env::var(name).ok())?;
                with_shutdown(|stop| fridica::daemon::observe(config, credentials, stop)).await?;
            }
        }
        Command::Control(command) => {
            println!("{}", serde_json::to_string_pretty(&command.run().await?)?)
        }
        Command::CheckConfig { config } => {
            let config = fridica::config::load(&config, &fridica::config::LoadContext::current()?)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "fingerprint": config.fingerprint,
                    "machines": config.machines.names(),
                    "default_machine": config.parent.default_machine,
                    "attention": config.attention,
                    "isolation": config.isolation.summary(&config.machines),
                }))?
            );
        }
        Command::DoctorIsolation {
            config,
            machine,
            workspace,
            timeout,
        } => {
            let context = fridica::config::LoadContext::current()?;
            let config = fridica::config::load(&config, &context)?;
            let report = fridica::doctor::isolation(
                &config,
                &context,
                std::env::vars_os().collect(),
                &machine,
                &workspace,
                std::time::Duration::from_secs(timeout),
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            if !report.passed() {
                anyhow::bail!("worker isolation preflight did not pass");
            }
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
                ("template.toml", include_bytes!("../config/template.toml")),
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

/// Install handlers before starting probes or durable/network service effects.
/// The operation owns cleanup after stop; never abandon its future on a signal.
async fn with_shutdown<T, F, Fut>(run: F) -> Result<T>
where
    F: FnOnce(tokio::sync::watch::Receiver<bool>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let (stop, stopping) = tokio::sync::watch::channel(false);
    let running = run(stopping);
    tokio::pin!(running);
    tokio::select! {
        result=&mut running => result,
        _=async { tokio::select! { _=interrupt.recv()=>{}, _=terminate.recv()=>{} } } => {
            stop.send_replace(true);
            running.await
        },
    }
}
