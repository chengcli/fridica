//! How worker processes launch: locally or over SSH, confined or not, with the
//! isolation, credential and Python-environment steps each placement needs.
use super::protocol::*;
use crate::{
    config::Config,
    core::{delivery::AdapterFuture, worker::*},
    exec::{
        local::LocalTransport,
        process::{self, Launch},
        ssh::{LaunchOptions, SshTransport},
    },
};
use anyhow::Context;
use serde_json::json;
use std::{collections::BTreeMap, ffi::OsString, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub trait Launcher: Send + Sync {
    /// Mutating setup, called only after read-only admission succeeds.
    fn prepare(&self, _spec: WorkerSpec) -> AdapterFuture<'_, ()> {
        Box::pin(async {})
    }
    fn validate_config(&self, _config: &Config) -> anyhow::Result<()> {
        Ok(())
    }
    fn admit(
        &self,
        _config: Arc<Config>,
        _spec: WorkerSpec,
    ) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
    }

    fn launch(&self, spec: &WorkerSpec, command: Vec<String>) -> Result<Launch, WorkerFailure>;
}
/// Explicit owner environment snapshot; no process-global environment mutation.
pub struct SystemLauncher {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub ssh_control_directory: PathBuf,
    pub isolation: crate::exec::isolation::Isolation,
    // Independent bounded probe capacity survives dropped admission futures.
    probes: Arc<Semaphore>,
    keyring_names: std::sync::OnceLock<Vec<String>>,
    native_config: Config,
}
impl SystemLauncher {
    /// Best-effort retention after admission, before inputs or backend work
    /// begin. The supervisor holds the slot exclusively through preparation.
    async fn cleanup(&self, spec: &WorkerSpec) {
        if !spec.create_cwd()
            || spec.workspace.policy.mode == "read-only"
            || spec.workspace.path.file_name().and_then(|s| s.to_str())
                != Some(format!("worker{}", spec.slot).as_str())
        {
            return;
        }
        let command = crate::exec::helper::argv(vec![
            include_str!("../exec/cleanup.py").into(),
            json!({"workspace":spec.workspace.path,"slot":spec.slot}).to_string(),
        ]);
        if let Ok(launch) = self.launch(spec, command) {
            let _ = process::run_with_open_stdin(launch, Duration::from_secs(30), 4096).await;
        }
    }
    /// Construct from owner configuration, never from a worker/model payload.
    pub fn from_config(
        config: &Config,
        home: PathBuf,
        environment: BTreeMap<OsString, OsString>,
        ssh_control_directory: PathBuf,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            home,
            environment,
            ssh_control_directory,
            isolation: crate::exec::isolation::Isolation::new(config, &[])?,
            probes: Arc::new(Semaphore::new(1)),
            keyring_names: Default::default(),
            native_config: config.clone(),
        })
    }
}
impl Launcher for SystemLauncher {
    fn prepare(&self, spec: WorkerSpec) -> AdapterFuture<'_, ()> {
        Box::pin(async move { self.cleanup(&spec).await })
    }
    fn validate_config(&self, config: &Config) -> anyhow::Result<()> {
        let current = crate::exec::isolation::Isolation::new(config, &[])?;
        if current != self.isolation
            || config.overleaf != self.native_config.overleaf
            || config.slack.channels != self.native_config.slack.channels
            || config.slack.workspace != self.native_config.slack.workspace
            || config.machines != self.native_config.machines
        {
            anyhow::bail!("worker isolation configuration changed; rebuild runtime adapters");
        }
        Ok(())
    }
    fn admit(
        &self,
        config: Arc<Config>,
        spec: WorkerSpec,
    ) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async move {
            self.validate_config(&config)
                .map_err(|_| failure(Failure::Refusal, "worker_isolation_configuration_changed"))?;
            if crate::github::mcp::target(&config, &spec.machine.name, &spec.workspace.policy)
                .is_some()
            {
                let launch = self.launch(
                    &spec,
                    crate::github::mcp::configured_probe_command(
                        &config,
                        &spec.machine.name,
                        &spec.workspace.policy,
                    ),
                )?;
                if !crate::github::mcp::probe_launch(launch, Duration::from_secs(30)).await {
                    return Err(failure(Failure::Refusal, "worker_github_mcp_unavailable"));
                }
            }
            if !spec.confined() {
                return Ok(());
            }
            // Probe the configured base: slot directories can be absent until
            // launch. The actual slot/settings are checked again by the launch
            // helper, which alone may create directories. No probe is cached.
            let base = config
                .machines
                .get(&spec.machine.name)
                .and_then(|m| m.workspace(&spec.workspace.name))
                .ok_or_else(|| failure(Failure::Refusal, "worker_isolation_target_unknown"))?;
            let permit = self
                .probes
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| failure(Failure::Refusal, "worker_isolation_probe_unavailable"))?;
            let mut probe_environment = self.environment.clone();
            probe_environment.remove(std::ffi::OsStr::new(crate::config::overleaf::NATIVE_ENV));
            let mut probe_excluded = spec.excluded_env.clone();
            probe_excluded.push(crate::config::overleaf::NATIVE_ENV.into());
            let launch = match spec.machine.transport.as_str() {
                "local" => self.isolation.preflight(
                    &LocalTransport {
                        machine: spec.machine.clone(),
                        home: self.home.clone(),
                        excluded_env: probe_excluded.clone(),
                    },
                    &base.path,
                    probe_environment.clone(),
                ),
                "ssh" => {
                    let mut launch = self.isolation.preflight_remote(
                        &SshTransport {
                            machine: spec.machine.clone(),
                            excluded_env: probe_excluded.clone(),
                            control_directory: self.ssh_control_directory.clone(),
                        },
                        &base.path.to_string_lossy(),
                        probe_environment.clone(),
                    );
                    if let Ok(launch) = &mut launch {
                        crate::exec::isolation::read_only_ssh_probe(launch);
                    }
                    launch
                }
                _ => {
                    return Err(failure(
                        Failure::Refusal,
                        "worker_isolation_transport_unsupported",
                    ))
                }
            }
            .map_err(|_| failure(Failure::Refusal, "worker_isolation_launch_refused"))?;
            let launch = crate::exec::python::activate(
                launch,
                &spec.machine.transport,
                &spec.python_env,
                Some(&self.home),
            )
            .map_err(|_| failure(Failure::Refusal, "worker_python_environment_invalid"))?;
            let check = crate::exec::isolation::run_probe_with_permit(
                launch,
                Duration::from_secs(30),
                permit,
            )
            .await;
            use crate::exec::isolation::Check;
            let code = match check {
                Check::Passed => return Ok(()),
                Check::InventoryRefused => "worker_isolation_inventory_refused",
                Check::SettingsRefused => "worker_isolation_settings_refused",
                Check::NamespaceFailed => "worker_isolation_namespace_failed",
                Check::RuntimeOrTransportFailed => "worker_isolation_runtime_or_transport_failed",
                _ => "worker_isolation_probe_failed",
            };
            Err(failure(Failure::Refusal, code))
        })
    }

    fn launch(&self, spec: &WorkerSpec, command: Vec<String>) -> Result<Launch, WorkerFailure> {
        use crate::config::overleaf::NATIVE_ENV;
        use crate::core::secret::SecretStore;
        let native_allowed = super::overleaf::server(&self.native_config, spec)
            .is_some_and(|expected| spec.mcp_servers.iter().any(|s| s == &expected));
        let mut spec = spec.clone();
        if !native_allowed {
            spec.excluded_env.push(NATIVE_ENV.into());
        }
        let mut environment = process::scrubbed_environment(
            self.environment.clone(),
            &spec.excluded_env,
            &BTreeMap::new(),
        );
        crate::secrets::worker_tokens(
            &mut environment,
            &spec.mcp_servers,
            native_allowed,
            |entry| {
                let store = crate::secrets::NativeStore;
                if entry == crate::config::overleaf::NATIVE_ENTRY {
                    return store.get(entry);
                }
                let names = self
                    .keyring_names
                    .get_or_init(|| store.list().unwrap_or_default());
                if names.iter().any(|name| name == entry) {
                    store.get(entry)
                } else {
                    Ok(None)
                }
            },
        )
        .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
        if native_allowed {
            if let Some(raw) = environment
                .get(std::ffi::OsStr::new(NATIVE_ENV))
                .and_then(|value| value.to_str())
            {
                let mut scope = self
                    .native_config
                    .overleaf
                    .native_scope
                    .clone()
                    .expect("authorized scope");
                if let Some(project) = &spec.thread_project {
                    scope.project = Some(project.clone());
                }
                let (saved_origin, _, _) = crate::config::overleaf::session_policy(raw)
                    .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                let configured_origin = fridica_agent::overleaf::login::normalize_base_url(
                    self.native_config
                        .overleaf
                        .login
                        .as_ref()
                        .and_then(|login| login.base_url.as_deref())
                        .unwrap_or("https://www.overleaf.com"),
                )
                .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                if saved_origin != configured_origin {
                    return Err(failure(
                        Failure::Refusal,
                        "worker_mcp_credential_unavailable",
                    ));
                }
                let raw = crate::config::overleaf::bound_session(
                    raw,
                    &super::overleaf::recipient(&self.native_config, &spec)
                        .expect("authorized recipient"),
                    self.native_config.overleaf.native_recipient.as_ref(),
                    &scope,
                    spec.native_channel.as_deref().expect("admitted channel"),
                )
                .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                let mut raw: serde_json::Value = serde_json::from_str(&raw)
                    .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                let mut legacy = crate::config::overleaf::legacy_workspaces(&raw.to_string())
                    .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                legacy.extend(
                    self.native_config
                        .machines
                        .get(&spec.machine.name)
                        .expect("registered machine")
                        .workspaces
                        .iter()
                        .map(|w| w.path.clone()),
                );
                legacy.sort();
                legacy.dedup();
                raw["legacy_workspaces"] = serde_json::json!(legacy);
                let raw = raw.to_string();
                crate::config::overleaf::validate_session(&raw)
                    .map_err(|_| failure(Failure::Refusal, "worker_mcp_credential_unavailable"))?;
                environment.insert(NATIVE_ENV.into(), raw.into());
            }
        }
        let native_available =
            native_allowed && environment.contains_key(std::ffi::OsStr::new(NATIVE_ENV));
        if !native_available && !spec.excluded_env.iter().any(|n| n == NATIVE_ENV) {
            spec.excluded_env.push(NATIVE_ENV.into());
        }
        // This inner command guard also strips target-side ambient credentials after
        // Python activation. Confined/Codex-startup helpers use excluded_env instead.
        let guarded = |command: Vec<String>| {
            if native_available {
                command
            } else {
                let mut words = vec!["/usr/bin/env".into(), "-u".into(), NATIVE_ENV.into()];
                words.extend(command);
                words
            }
        };
        let launch = match spec.machine.transport.as_str() {
            "local" => {
                let transport = LocalTransport {
                    machine: spec.machine.clone(),
                    home: self.home.clone(),
                    excluded_env: spec.excluded_env.clone(),
                };
                if spec.confined() {
                    self.isolation.launch(
                        &transport,
                        command,
                        &spec.workspace.path,
                        environment,
                        spec.create_cwd(),
                    )
                } else if command.first().is_some_and(|s| s == "codex")
                    && command.get(1).is_some_and(|s| s == "app-server")
                {
                    self.isolation
                        .mcp_startup(
                            command,
                            Some(&self.home),
                            &spec.workspace.path,
                            &spec.excluded_env,
                            spec.create_cwd(),
                        )
                        .and_then(|command| {
                            transport.launch(
                                command,
                                std::path::Path::new("/"),
                                environment,
                                &BTreeMap::new(),
                                None,
                                false,
                            )
                        })
                } else {
                    transport.launch(
                        guarded(command),
                        &spec.workspace.path,
                        environment,
                        &BTreeMap::new(),
                        None,
                        spec.create_cwd(),
                    )
                }
            }
            "ssh" => {
                let transport = SshTransport {
                    machine: spec.machine.clone(),
                    excluded_env: spec.excluded_env.clone(),
                    control_directory: self.ssh_control_directory.clone(),
                };
                if spec.confined() {
                    self.isolation.launch_remote(
                        &transport,
                        command,
                        &spec.workspace.path.to_string_lossy(),
                        environment,
                        spec.create_cwd(),
                    )
                } else if command.first().is_some_and(|s| s == "codex")
                    && command.get(1).is_some_and(|s| s == "app-server")
                {
                    self.isolation
                        .mcp_startup(
                            command,
                            None,
                            &spec.workspace.path,
                            &spec.excluded_env,
                            spec.create_cwd(),
                        )
                        .and_then(|command| {
                            transport.launch(
                                command,
                                "/",
                                environment,
                                &BTreeMap::new(),
                                LaunchOptions::default(),
                            )
                        })
                } else {
                    transport.launch(
                        guarded(command),
                        &spec.workspace.path.to_string_lossy(),
                        environment,
                        &BTreeMap::new(),
                        LaunchOptions {
                            create: spec.create_cwd(),
                            ..LaunchOptions::default()
                        },
                    )
                }
            }
            _ => return Err(failure(Failure::Refusal, "transport_unavailable")),
        };
        launch
            .and_then(|mut launch| {
                if native_available {
                    // Outside the existing startup/isolation helper, after Python activation:
                    // the Rust broker consumes the grant, then execs that same guarded launch.
                    let executable = super::overleaf::server(&self.native_config, &spec)
                        .expect("authorized native server")
                        .command;
                    let mut prefix = vec![
                        executable,
                        "overleaf-run".into(),
                        "--workspace".into(),
                        spec.workspace.path.to_string_lossy().into_owned(),
                        "--worker".into(),
                        spec.worker_id.clone(),
                        "--channel".into(),
                        spec.native_channel.clone().expect("admitted channel"),
                        "--project".into(),
                        spec.thread_project
                            .clone()
                            .unwrap_or_else(|| "unbound".into()),
                        "--".into(),
                    ];
                    if spec.machine.transport == "local" {
                        prefix.extend(launch.argv);
                        launch.argv = prefix;
                    } else {
                        let script = launch.argv.last_mut().context("missing SSH command")?;
                        prefix.extend(["/bin/sh".into(), "-c".into(), script.clone()]);
                        *script = crate::exec::shell::join(&prefix);
                    }
                }
                let launch = crate::exec::python::activate(
                    launch,
                    &spec.machine.transport,
                    &spec.python_env,
                    Some(&self.home),
                )?;
                if spec.machine.transport == "ssh" {
                    let names = spec
                        .mcp_servers
                        .iter()
                        .flat_map(|s| s.env_vars.iter().cloned())
                        .collect::<Vec<_>>();
                    crate::exec::credentials::forward(launch, &spec.excluded_env, &names)
                } else {
                    Ok(launch)
                }
            })
            .map_err(|_| failure(Failure::Refusal, "backend_launch_configuration_failed"))
    }
}
