use anyhow::Result;
use clap::{Parser, Subcommand};
use fridica::{
    core::time::{Clock, SystemClock},
    store::Store,
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
    /// Manage Fridica credentials in the current user's OS credential store.
    #[command(subcommand)]
    Secret(fridica::cli::secret::Command),
    /// Manage the dedicated scoped Overleaf web session.
    #[command(subcommand)]
    OverleafAuth(fridica::cli::overleaf_auth::Command),
    /// Serve the bundled GitHub MCP tools over stdio on a worker machine.
    GithubMcp(GithubMcpArgs),
    /// Serve the bundled alphaXiv literature research MCP tools over stdio.
    AlphaxivMcp {
        /// Check authentication and tool discovery without running a search.
        #[arg(long)]
        probe: bool,
    },
    /// Serve NASA ADS literature and library tools over stdio.
    ScixMcp {
        /// Verify the token with an ADS query requesting zero results.
        #[arg(long)]
        probe: bool,
    },
    /// Serve the bundled Overleaf project tools over stdio.
    OverleafMcp {
        /// Confine the server and its subprocesses to writes in this worker workspace.
        #[arg(long, value_name = "PATH")]
        workspace_write: Option<PathBuf>,
        /// Project selected by the Slack thread for this worker job.
        #[arg(long, hide = true)]
        thread_project: Option<String>,
        /// Trusted launcher context for native tools (no owner login on workers).
        #[arg(long, hide = true, requires_all = ["native_channel", "thread_project"], conflicts_with = "probe")]
        native_worker: Option<String>,
        #[arg(long, hide = true, requires = "native_worker")]
        native_channel: Option<String>,
        /// Inner sandbox entry, never an owner login.
        #[arg(long, hide = true, requires_all = ["native_worker", "native_channel", "thread_project"])]
        native_serve: bool,
        #[arg(long, hide = true)]
        native_discovery: bool,
        #[arg(long, hide = true, requires = "native_worker")]
        native_session_stdin: bool,
        /// Discover native and built-in tools without reading or forwarding a session.
        #[arg(long, hide = true, requires_all = ["probe", "workspace_write"], conflicts_with = "native_worker")]
        native_probe: bool,
        /// Check bundled tools and verify project read access when OVERLEAF_PROJECT_ID is set.
        #[arg(long)]
        probe: bool,
    },
    #[command(hide = true)]
    OverleafRun {
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        worker: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        project: String,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    #[command(hide = true)]
    OverleafBrokerServe {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        worker: String,
        #[arg(long)]
        channel: String,
        #[arg(long)]
        project: String,
    },
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
        /// Load credentials from a systemd environment file.
        #[arg(long, conflicts_with = "summon_file")]
        environment_file: Option<PathBuf>,
        /// Launch through Summon with this absolute path to a provider reference mapping.
        #[arg(long)]
        summon_file: Option<PathBuf>,
        /// Absolute path to a Summon provider, such as the installed fridica-secret.
        #[arg(long, requires = "summon_file")]
        summon_provider: Option<PathBuf>,
        /// Print an observe-only unit instead of the active default.
        #[arg(long)]
        observe_only: bool,
    },
    /// Create a starter configuration, contract, Slack manifest and Summon mapping.
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
        /// Check Slack token format without contacting Slack.
        #[arg(long)]
        local_slack: bool,
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
    /// Serve the control-room dashboard on 127.0.0.1 for the running daemon.
    ///
    /// Prints a URL with a fresh key for this run; open it in a browser on
    /// this machine. Stops on Ctrl-C.
    Dashboard {
        #[arg(long, conflicts_with = "socket")]
        config: Option<std::path::PathBuf>,
        /// The daemon's control socket, instead of the configuration's.
        #[arg(long)]
        socket: Option<std::path::PathBuf>,
        /// Port on 127.0.0.1; 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
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
#[derive(clap::Args)]
struct GithubMcpArgs {
    /// Restrict the server to read tools.
    #[arg(long)]
    read_only: bool,
    /// Target-local environment variable holding the GitHub token.
    #[arg(long, default_value = "GH_TOKEN")]
    token_env: String,
    /// Read authenticated identity and exit without publishing.
    #[arg(long)]
    probe: bool,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    fridica::exec::tls::initialize();
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
        Command::OverleafAuth(command) => {
            tokio::task::spawn_blocking(move || fridica::cli::overleaf_auth::run(command))
                .await??;
        }
        Command::Secret(command) => {
            tokio::task::spawn_blocking(move || fridica::cli::secret::run(command)).await??;
        }
        Command::AlphaxivMcp { probe } => {
            if probe {
                fridica_agent::alphaxiv::probe().await?;
                println!("fridica-alphaxiv-mcp:ready");
            } else {
                fridica_agent::alphaxiv::run().await?;
            }
        }
        Command::ScixMcp { probe } => {
            if probe {
                fridica_agent::scix::probe().await?;
                println!("fridica-scix-mcp:ready");
            } else {
                tokio::task::spawn_blocking(fridica_agent::scix::run).await??;
            }
        }
        Command::OverleafMcp {
            probe,
            workspace_write,
            thread_project,
            native_worker,
            native_channel,
            native_probe,
            native_serve,
            native_discovery,
            native_session_stdin,
        } => {
            if native_discovery {
                return fridica::overleaf::runtime::discover_native();
            }
            if native_serve {
                anyhow::ensure!(
                    native_session_stdin,
                    "native session must use the memory pipe"
                );
                return fridica::overleaf::runtime::serve_native(
                    native_worker.as_deref().expect("native worker"),
                    native_channel.as_deref().expect("native channel"),
                    thread_project.as_deref().expect("native project"),
                );
            }
            if native_probe {
                return fridica::overleaf::runtime::probe_native(
                    workspace_write.as_deref().expect("probe workspace"),
                );
            }
            if let (Some(worker), Some(channel), Some(root)) =
                (&native_worker, &native_channel, &workspace_write)
            {
                return fridica::overleaf::runtime::launch_native(
                    root,
                    worker,
                    channel,
                    thread_project.as_deref().expect("native project"),
                    native_session_stdin,
                );
            }
            anyhow::ensure!(
                native_worker.is_none(),
                "native tools require the workspace sandbox"
            );
            if let Some(root) = workspace_write {
                return fridica::overleaf::runtime::launch(&root, probe, thread_project.as_deref());
            }
            if let Some(project) = thread_project {
                return fridica::overleaf::runtime::serve_project(&project);
            }
            if probe {
                if fridica::research::probe_overleaf().await? {
                    println!("fridica-overleaf-mcp:authenticated");
                } else {
                    println!("fridica-overleaf-mcp:ready");
                }
            } else {
                tokio::task::spawn_blocking(fridica_agent::overleaf::run).await??;
            }
        }
        Command::OverleafRun {
            workspace,
            worker,
            channel,
            project,
            command,
        } => {
            return fridica::overleaf::broker::run(
                &workspace, &worker, &channel, &project, &command,
            )
        }
        Command::OverleafBrokerServe {
            socket,
            workspace,
            worker,
            channel,
            project,
        } => {
            return fridica::overleaf::broker::serve(
                &socket, &workspace, &worker, &channel, &project,
            )
        }
        Command::GithubMcp(options) => {
            fridica_agent::github::run(fridica_agent::github::Options {
                read_only: options.read_only,
                token_env: options.token_env,
                probe: options.probe,
            })
            .await?;
        }
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
            summon_file,
            summon_provider,
            observe_only,
        } => {
            let executable = std::env::current_exe()?;
            let config = config_path(config)?;
            let unit = match (summon_file, environment_file) {
                (Some(mapping), _) => fridica::cli::candidate::summon_service(
                    &executable,
                    &config,
                    &mapping,
                    summon_provider.as_deref(),
                    observe_only,
                )?,
                (None, None) => {
                    fridica::cli::candidate::native_service(&executable, &config, observe_only)?
                }
                (None, Some(environment)) => fridica::cli::candidate::service(
                    &executable,
                    &config,
                    &environment,
                    observe_only,
                )?,
            };
            print!("{unit}");
        }
        Command::Init { config } => {
            let context = fridica::config::LoadContext::current()?;
            let path = fridica::config::setup::path(config.as_deref(), &context)?;
            fridica::config::setup::init_with(&path, fridica::secrets::starter_config)?;
            println!(
                "Created {}. Agent rules are in {}.",
                path.display(),
                path.parent().unwrap().join("contract.md").display()
            );
            println!("MCP settings reflect the credentials found during setup. Next: use this executable with the same --config path for configure --detect, doctor and start --check-ready; complete the workspaces first. `start` runs the daemon; add --observe-only to record without replying.");
        }
        Command::Configure(options) => options
            .run(&fridica::config::LoadContext::current()?)
            .await
            .map_err(fridica::cli::input_error)?,
        Command::Start {
            config,
            observe_only,
            check_ready,
            timeout,
        } => fridica::cli::start::run(config, observe_only, check_ready, timeout).await?,
        Command::Control(command @ fridica::control::cli::Commands::Events { .. }) => command
            .events()
            .await
            .map_err(fridica::cli::control_error)?,
        Command::Control(command) => {
            let result = command.run().await.map_err(fridica::cli::control_error)?;
            println!("{}", serde_json::to_string_pretty(&result)?)
        }
        Command::Doctor {
            local_slack,
            config,
            json,
            timeout,
        } => fridica::cli::doctor::run(config, json, timeout, local_slack).await?,
        Command::Migrate {
            database,
            config,
            dry_run,
            rollback,
        } => {
            let config = config_path(config)?;
            let database = database_path(database, &config)?;
            if rollback {
                fridica::cli::migrate::rollback(&database, &config)?;
                println!("Restored migration backups.");
            } else {
                let plan = if dry_run {
                    fridica::cli::migrate::dry_run(&database, &config)?
                } else {
                    fridica::cli::migrate::migrate(&database, &config, SystemClock.now())?
                };
                println!("{}", serde_json::to_string_pretty(&plan)?);
            }
        }
        Command::Dashboard {
            config,
            socket,
            port,
        } => fridica::cli::dashboard::run(config, socket, port).await?,
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
