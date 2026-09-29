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
    about = "Experimental v0.4 candidate. Separate from the production Python launcher."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Print the candidate version, source fingerprint and native target.
    BuildInfo,
    /// Initialize fresh v6 state offline; existing legacy databases require migrate.
    InitState {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Print a user systemd service; default mode is observe-only. Does not install it.
    ServicePrint {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        environment_file: PathBuf,
        #[arg(long)]
        deployment_record: Option<PathBuf>,
    },
    /// Record the owner's completed deployment checks for this build/config/host.
    DeploymentRecord {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, required = true)]
        target_conformance: bool,
        #[arg(long, required = true)]
        recovery_rehearsal: bool,
        #[arg(long, required = true)]
        observe_only_reconciled: bool,
    },
    /// Create an experimental starter configuration, contract and Slack manifest.
    Init {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// Fill Slack identity and channel settings before the configuration is complete.
    Configure(fridica::cli::setup::Configure),
    #[command(flatten)]
    Control(fridica::control::cli::Commands),
    /// Candidate daemon with owner-authenticated local controls.
    Start {
        #[arg(long)]
        config: Option<PathBuf>,
        /// Start without model, worker or posting adapters.
        #[arg(long)]
        observe_only: bool,
        /// Explicit opt-in after deployment checks recorded by the owner.
        #[arg(long, conflicts_with_all = ["observe_only", "check_ready"], requires = "deployment_record")]
        active: bool,
        #[arg(long, requires = "active")]
        deployment_record: Option<PathBuf>,
        /// Read-only startup preparation; no Slack credentials or state required.
        #[arg(long, conflicts_with = "observe_only")]
        check_ready: bool,
        #[arg(long, requires = "check_ready", value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout: Option<u64>,
    },
    /// Validate configuration and resolved placement policies without starting adapters.
    CheckConfig {
        #[arg(long)]
        config: Option<PathBuf>,
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
    /// Test worker isolation on one explicit local/SSH target without a backend.
    DoctorIsolation {
        #[arg(long)]
        config: Option<PathBuf>,
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
        config: Option<PathBuf>,
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
            deployment_record,
        } => {
            print!(
                "{}",
                fridica::cli::candidate::service(
                    &std::env::current_exe()?,
                    &config_path(config)?,
                    &environment_file,
                    deployment_record.as_deref()
                )?
            );
        }
        Command::DeploymentRecord {
            config,
            output,
            target_conformance: _,
            recovery_rehearsal: _,
            observe_only_reconciled: _,
        } => {
            let config = fridica::config::load(
                &config_path(config)?,
                &fridica::config::LoadContext::current()?,
            )
            .map_err(fridica::cli::input_error)?;
            fridica::cli::candidate::write_attestation(
                &config,
                &output,
                fridica::cli::candidate::Build::current(),
                SystemClock.now(),
            )?;
            println!(
                "Recorded owner deployment attestation at {}",
                output.display()
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
            println!("Next: use this executable with the same --config path for configure --detect, check-config and start --check-ready; complete workspaces and inventories first. Services default to observe-only; active startup requires an owner deployment record.");
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
            active,
            deployment_record,
            check_ready,
            timeout,
        } => {
            if !observe_only && !check_ready && !active {
                anyhow::bail!("experimental Rust start requires --observe-only or explicit --active with --deployment-record; use --check-ready for startup preparation");
            }
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
                if active {
                    fridica::cli::candidate::validate(
                        &config,
                        &fridica::cli::candidate::Build::current(),
                        deployment_record.as_deref().unwrap(),
                    )?;
                }
                let credentials =
                    fridica::daemon::Credentials::read(&config, |name| std::env::var(name).ok())
                        .map_err(fridica::cli::input_error)?;
                if active {
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
        Command::CheckConfig { config } => {
            let config = fridica::config::load(
                &config_path(config)?,
                &fridica::config::LoadContext::current()?,
            )
            .map_err(fridica::cli::input_error)?;
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
        Command::Doctor {
            config,
            json,
            timeout,
        } => {
            let context = fridica::config::LoadContext::current()?;
            let path = fridica::config::setup::path(config.as_deref(), &context)?;
            let report = with_shutdown(|stop| {
                fridica::doctor::checks::run(
                    &path,
                    &context,
                    std::env::vars_os().collect(),
                    std::time::Duration::from_secs(timeout),
                    stop,
                )
            })
            .await?;
            println!(
                "{}",
                if json {
                    serde_json::to_string_pretty(&report)?
                } else {
                    report.text()
                }
            );
            if !report.passed() {
                anyhow::bail!("doctor checks did not pass");
            }
        }
        Command::DoctorIsolation {
            config,
            machine,
            workspace,
            timeout,
        } => {
            let context = fridica::config::LoadContext::current()?;
            let config = fridica::config::load(&config_path(config)?, &context)?;
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
            let config = config_path(config)?;
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
