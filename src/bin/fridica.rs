use anyhow::Result;
use clap::{Parser, Subcommand};
use fridica::{
    core::time::{Clock, SystemClock},
    report,
    store::{migration, Store},
};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    version,
    about = "Slack agent daemon: a parent agent that answers threads and delegates work to Claude Code/Codex workers."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Print the version, source fingerprint and native target.
    BuildInfo,
    /// Initialize fresh v6 state offline; existing legacy databases require migrate.
    InitState {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a user systemd service (active unless --observe-only). Does not install it.
    ServicePrint {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        environment_file: PathBuf,
        /// Print an observe-only unit instead of the active default.
        #[arg(long)]
        observe_only: bool,
    },
    /// Create a starter configuration, contract and Slack manifest.
    Init {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Fill Slack identity and channel settings before the configuration is complete.
    Configure(fridica::cli::setup::Configure),
    #[command(flatten)]
    Control(fridica::control::cli::Commands),
    /// Run the daemon, with owner-authenticated local controls.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Connect and record intake without model, worker or posting adapters.
        #[arg(long)]
        observe_only: bool,
        /// Read-only startup preparation; no Slack credentials or state required.
        #[arg(long, conflicts_with = "observe_only")]
        check_ready: bool,
        #[arg(long, requires = "check_ready", value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout: Option<u64>,
    },
    /// Check configuration, tokens, backend sign-in and protocols without model requests.
    Doctor {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout: u64,
    },
    /// Migrate a stopped daemon's database and configuration, with backups.
    /// Without --database, the configuration's state.path is migrated.
    Migrate {
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, conflicts_with = "rollback")]
        dry_run: bool,
        #[arg(long)]
        rollback: bool,
    },
    /// Generate and atomically export a report from an offline database
    /// (the configuration's state.path unless --database says otherwise).
    Report {
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        date: chrono::NaiveDate,
        #[arg(long)]
        timezone: chrono_tz::Tz,
        #[arg(long)]
        directory: PathBuf,
    },
    /// Show the embedded assets used by this build.
    Assets {
        #[arg(long)]
        list: bool,
        /// Export all embedded assets into a new directory.
        #[arg(long, conflicts_with = "list")]
        export: Option<PathBuf>,
    },
}
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fridica: {error}");
            std::process::ExitCode::from(fridica::cli::exit_code(&error))
        }
    }
}
/// `--config`, or `~/.config/fridica/config.toml` like `init` and `start`.
fn config_path(config: Option<PathBuf>) -> Result<PathBuf> {
    fridica::config::setup::path(config.as_deref(), &fridica::config::LoadContext::current()?)
}
/// An explicit database, else the one the configuration names.
fn database_path(database: Option<PathBuf>, config: &Path) -> Result<PathBuf> {
    match database {
        Some(database) => Ok(database),
        None => {
            let context = fridica::config::LoadContext::current()?;
            Ok(fridica::config::loader::load(config, &context)?.state.path)
        }
    }
}
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::BuildInfo => println!(
            "{}",
            serde_json::to_string_pretty(&fridica::cli::candidate::Build::current())?
        ),
        Command::InitState { config } => {
            let config = fridica::config::load(
                &config_path(config)?,
                &fridica::config::LoadContext::current()?,
            )
            .map_err(fridica::cli::input_error)?;
            let _store = Store::open(config.state.path.clone()).await?;
            println!("Initialized v6 state at {}", config.state.path.display());
        }
        Command::ServicePrint {
            config,
            environment_file,
            observe_only,
        } => {
            print!(
                "{}",
                fridica::cli::candidate::service(
                    &std::env::current_exe()?,
                    &config_path(config)?,
                    &environment_file,
                    observe_only
                )?
            );
        }
        Command::Init { config } => {
            let context = fridica::config::LoadContext::current()?;
            let path = fridica::config::setup::path(config.as_deref(), &context)?;
            fridica::config::setup::init(&path)?;
            println!(
                "Created {}. Agent rules are in {}.",
                path.display(),
                path.parent().unwrap().join("contract.md").display()
            );
            println!("Next: use this executable with the same --config path for configure --detect, doctor and start --check-ready; complete the workspaces first. `start` runs the daemon; add --observe-only to record without replying.");
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
            let context = fridica::config::LoadContext::current()?;
            let path = config.unwrap_or_else(|| context.home.join(".config/fridica/config.toml"));
            let config =
                fridica::config::load(&path, &context).map_err(fridica::cli::input_error)?;
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
                    fridica::daemon::Credentials::read(&config, |name| std::env::var(name).ok())
                        .map_err(fridica::cli::input_error)?;
                if !observe_only {
                    // Active by default: readiness and doctor rerun before Slack I/O.
                    with_shutdown(|stop| {
                        fridica::daemon::active(
                            config,
                            context,
                            std::env::vars_os().collect(),
                            credentials,
                            stop,
                        )
                    })
                    .await?;
                } else {
                    with_shutdown(|stop| fridica::daemon::observe(config, credentials, stop))
                        .await?;
                }
            }
        }
        Command::Control(command) => {
            let result = command.run().await.map_err(fridica::cli::control_error)?;
            println!("{}", serde_json::to_string_pretty(&result)?)
        }
        Command::Doctor {
            config,
            json,
            timeout,
        } => {
            let context = fridica::config::LoadContext::current()?;
            let path = fridica::config::setup::path(config.as_deref(), &context)?;
            use fridica::doctor::checks::{self, Progress};
            // Each result is shown as soon as it is known: machines are probed
            // at the same time, and a slow host should not look like a hang.
            // JSON stays whole on stdout, so there the lines go to stderr.
            let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
            let printer = tokio::spawn(async move {
                while let Some(event) = receiver.recv().await {
                    let line = match event {
                        Progress::Configuration(c) => c.text(),
                        Progress::Check(c) => c.text(),
                    };
                    if json {
                        eprintln!("{line}");
                    } else {
                        println!("{line}");
                    }
                }
            });
            let report = with_shutdown(|stop| {
                checks::run_with(
                    &path,
                    &context,
                    std::env::vars_os().collect(),
                    std::time::Duration::from_secs(timeout),
                    stop,
                    Some(sender),
                )
            })
            .await;
            // Every sender is gone once the report is detached: the printer
            // then drains what is left and ends.
            let report = report.map(|mut report| {
                report.detach();
                report
            });
            printer.await?;
            let report = report?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report.summary());
            }
            if !report.passed() {
                anyhow::bail!("doctor checks did not pass");
            }
        }
        Command::Migrate {
            database,
            config,
            dry_run,
            rollback,
        } => {
            let config = config_path(config)?;
            let database = database_path(database, &config)?;
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
            let database = database_path(database, &config_path(None)?)?;
            let store = Store::open(database).await?;
            let data = report::generate(&store, channel, date, timezone, SystemClock.now()).await?;
            report::export_pending(&store, directory).await?;
            println!("{}", serde_json::to_string_pretty(&data)?);
        }
        Command::Assets { list: _, export } => {
            if let Some(path) = export {
                fridica::cli::assets::export(&path)?;
            }
            for (name, data) in fridica::cli::assets::catalog() {
                use sha2::{Digest, Sha256};
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
