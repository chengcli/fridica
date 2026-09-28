//! Shared observer/active library composition. Active CLI admission is still
//! gated; constructing these adapters is not a preflight or deployment approval.
use super::Disabled;
use crate::{
    config::Config,
    core::{
        delivery::{AdapterFuture, Delivery},
        parent::{Parent, ParentFailure, ParentRequest},
        time::{Clock, Identifiers},
    },
    exec::fetch::{Fetcher, SystemFetcher},
    github::{client, links::Links},
    parent::{self, CliParent},
    slack::{files::Downloader, links::Reader},
    store::Store,
    threads::runtime::{Adapters, Runtime},
    workers::{
        artifacts::SystemJobIo,
        fetch::ScopedJobIo,
        instructions::OwnerInstructions,
        jsonl::{self, BackendFactory, Launcher, StoreWireRecorder, SystemLauncher},
        protocol::{JobIo, NoJobIo},
    },
};
use anyhow::{bail, Result};
use serde_json::Value;
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, sync::Arc};

/// Preserve the parent's context-read allowance when erasing its adapter type.
pub struct ComposedParent(Arc<dyn Parent>);
impl Parent for ComposedParent {
    fn preparation_timeout(&self, request: &ParentRequest) -> std::time::Duration {
        self.0.preparation_timeout(request)
    }
    fn decide(&self, request: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        self.0.decide(request)
    }
}
pub type ComposedRuntime<S> = Runtime<ComposedParent, S>;

/// Trusted construction only. Tests replace process/network boundaries, while
/// exercising the actual parent, backend factory, instructions and JobIo stack.
/// Deliberately no Debug implementation: parent options contain owner secrets.
pub struct Execution {
    pub parent: parent::Options,
    pub launcher: Arc<dyn Launcher>,
    pub fetcher: Arc<dyn Fetcher>,
    pub artifacts: Arc<dyn JobIo>,
    pub github: Option<Arc<dyn client::Api>>,
    pub workers: jsonl::Options,
}
pub struct Host {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub ssh_control_directory: PathBuf,
    pub parent_temporary_root: PathBuf,
}
impl Execution {
    /// Build system adapters without starting processes or making API calls.
    /// Target directories/admission remain the responsibility of daemon startup.
    pub fn system(
        config: &Config,
        store: Store,
        clock: Arc<dyn Clock>,
        host: Host,
    ) -> Result<Self> {
        let github = if config.github.enabled {
            Some(Arc::new(
                client::Gh::with_environment(
                    config,
                    store,
                    clock,
                    client::Options::default(),
                    host.environment.clone(),
                )
                .map_err(|_| anyhow::anyhow!("GitHub adapter configuration failed"))?,
            ) as Arc<dyn client::Api>)
        } else {
            None
        };
        Ok(Self {
            artifacts: Arc::new(SystemJobIo {
                home: host.home.clone(),
                environment: host.environment.clone(),
                ssh_control_directory: host.ssh_control_directory.clone(),
                read_timeout: std::time::Duration::from_secs(30),
            }),
            launcher: Arc::new(SystemLauncher::from_config(
                config,
                host.home.clone(),
                host.environment.clone(),
                host.ssh_control_directory.clone(),
            )?),
            fetcher: Arc::new(SystemFetcher::new(
                host.home.clone(),
                host.environment.clone(),
                host.ssh_control_directory,
            )),
            parent: parent::Options {
                codex: "codex".into(),
                claude: "claude".into(),
                temporary_root: host.parent_temporary_root,
                environment: host.environment,
            },
            github,
            workers: jsonl::Options {
                disabled_mcp_servers: config.isolation.mcp_aliases.clone(),
                ..Default::default()
            },
        })
    }
}

pub enum Mode {
    /// No parent, worker, context or fetch adapter is constructed for observers.
    ObserveOnly,
    /// Library integration only until CLI parity/isolation admission gates pass.
    Active(Box<Execution>),
}

/// The same composition serves the observer CLI and active integration tests.
/// Store opening/migration and Slack authentication belong to the host service.
pub async fn start<S: Delivery + Downloader + Reader + 'static>(
    config: Arc<Config>,
    store: Store,
    slack: Arc<S>,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn Identifiers>,
    mode: Mode,
) -> Result<ComposedRuntime<S>> {
    let (adapters, observe_only) = match mode {
        Mode::ObserveOnly => (
            Adapters {
                parent: Arc::new(ComposedParent(Arc::new(Disabled))),
                delivery: slack,
                workers: Arc::new(Disabled),
                job_io: Arc::new(NoJobIo),
            },
            true,
        ),
        Mode::Active(execution) => {
            if config.github.enabled && execution.github.is_none() {
                bail!("enabled GitHub context requires an API adapter");
            }
            let mut parent = CliParent::new(
                config.clone(),
                store.clone(),
                clock.clone(),
                execution.parent,
            )
            .with_slack_context(slack.clone());
            if config.github.enabled {
                parent = parent.with_github(Arc::new(
                    Links::new(
                        execution.github.unwrap(),
                        store.clone(),
                        clock.clone(),
                        config.github.cache_seconds,
                    )
                    .map_err(|_| anyhow::anyhow!("GitHub context configuration failed"))?,
                ));
            }
            let factory = BackendFactory {
                launcher: execution.launcher,
                instructions: Arc::new(OwnerInstructions),
                ids: ids.clone(),
                options: execution.workers,
                recorder: Arc::new(StoreWireRecorder {
                    store: store.clone(),
                    clock: clock.clone(),
                }),
            };
            let io = ScopedJobIo {
                store: store.clone(),
                fetcher: execution.fetcher,
                artifacts: execution.artifacts,
                clock: clock.clone(),
            };
            (
                Adapters {
                    parent: Arc::new(ComposedParent(Arc::new(parent))),
                    delivery: slack,
                    workers: Arc::new(factory),
                    job_io: Arc::new(io),
                },
                false,
            )
        }
    };
    // Runtime supplies one durable approval broker to the supervisor and controls.
    Runtime::start(store, config, adapters, clock, ids, observe_only).await
}
