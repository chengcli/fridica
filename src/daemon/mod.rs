//! Candidate daemon composition. Active CLI execution requires an owner
//! deployment record and fresh read-only startup diagnostics.
pub mod composition;
pub mod log;
pub mod probe;
use crate::{
    config::Config,
    control::{
        api::Api,
        server::{Access, Options, Server},
        Backend,
    },
    core::{
        delivery::AdapterFuture,
        parent::{Parent, ParentFailure, ParentRequest},
        time::{RandomIds, SystemClock},
        worker::{Failure as WorkerFailureKind, WorkerFailure, WorkerRecord},
    },
    slack::socket,
    store::Store,
    threads::service::{self, Failure, Lifecycle},
    workers::protocol::{Factory, Worker, WorkerSpec},
};
use anyhow::{bail, Result};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::watch;

/// Owner/capability authority comes from trusted construction, never model or
/// HTTP request data. The service owns this object throughout cancellation.
pub struct ControlServer {
    config: Arc<Config>,
    backend: Arc<dyn Backend>,
    access: Option<Access>,
    options: Options,
    server: Option<Server>,
}
impl ControlServer {
    pub fn new(
        config: Arc<Config>,
        backend: Arc<dyn Backend>,
        access: Access,
        options: Options,
    ) -> Self {
        Self {
            config,
            backend,
            access: Some(access),
            options,
            server: None,
        }
    }
}
impl Lifecycle for ControlServer {
    fn start(&mut self) -> AdapterFuture<'_, Result<Option<PathBuf>, Failure>> {
        Box::pin(async move {
            let access = self.access.take().ok_or(Failure::HostService)?;
            self.server = Some(
                Server::bind(
                    &self.config,
                    self.backend.clone(),
                    access,
                    self.options.clone(),
                )
                .await
                .map_err(|_| Failure::HostService)?,
            );
            Ok(Some(self.config.state.control_socket.clone()))
        })
    }
    fn stop(&mut self) -> AdapterFuture<'_, Result<(), Failure>> {
        Box::pin(async move {
            if let Some(server) = self.server.take() {
                server.close().await.map_err(|_| Failure::HostService)?;
            }
            Ok(())
        })
    }
    fn failed(&self) -> AdapterFuture<'_, ()> {
        Box::pin(async move {
            if let Some(server) = &self.server {
                server.wait_stopped().await;
            }
        })
    }
}

/// Private, non-Debug credential holder. Reading credentials precedes opening
/// the state database, and errors identify settings without exposing values.
pub struct Credentials {
    app: String,
    user: String,
}
impl Credentials {
    pub fn read(config: &Config, env: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let read = |name: &str, prefix: &str| -> Result<String> {
            let token = env(name).unwrap_or_default();
            if !token.starts_with(prefix)
                || token.len() <= prefix.len()
                || !token.bytes().all(|b| b.is_ascii_graphic())
            {
                bail!("{name} must contain a valid Slack {prefix} token");
            }
            Ok(token)
        };
        Ok(Self {
            app: read(&config.slack.app_token_env, "xapp-")?,
            user: read(&config.slack.user_token_env, "xoxp-")?,
        })
    }
}

// In addition to Runtime's observe-only gates, the observer has no model or
// worker launch adapter. A regression cannot accidentally start a CLI backend.
struct Disabled;
impl Parent for Disabled {
    fn decide(&self, _: ParentRequest) -> AdapterFuture<'_, Result<Value, ParentFailure>> {
        Box::pin(async {
            Err(ParentFailure {
                code: "observe_only".into(),
            })
        })
    }
}
impl Factory for Disabled {
    fn instructions(&self, _: &Config, _: &WorkerRecord) -> Result<String> {
        bail!("observe-only worker execution is disabled")
    }
    fn create(&self, _: WorkerSpec) -> Result<Arc<dyn Worker>, WorkerFailure> {
        Err(WorkerFailure {
            kind: WorkerFailureKind::Refusal,
            code: "observe_only".into(),
            backend_session_id: String::new(),
        })
    }
}

/// Operates only on a fresh/v6 database; Store rejects legacy schemas and holds
/// the database lock. This does not migrate or launch an active worker runtime.
pub async fn observe(
    config: Config,
    credentials: Credentials,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    run(
        config,
        credentials,
        stop,
        None,
        crate::config::LoadContext::current()?,
    )
    .await
}

/// Active host wiring for the candidate; the CLI validates deployment authority.
/// Each invocation reruns preparation before database recovery or service I/O.
pub async fn active(
    config: Config,
    context: crate::config::LoadContext,
    environment: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    credentials: Credentials,
    stop: watch::Receiver<bool>,
) -> Result<()> {
    log::line(
        "INFO",
        "fridica",
        "checking machines, backends and credentials",
    );
    let report = crate::doctor::readiness::check(
        &config,
        &context,
        environment.clone(),
        Duration::from_secs(30),
        stop.clone(),
    )
    .await?;
    if report.cancelled {
        return Ok(());
    }
    if !report.startup_checks_passed {
        bail!("active startup readiness checks did not pass");
    }
    let diagnostics = crate::doctor::checks::run(
        &config.path,
        &context,
        environment.clone(),
        Duration::from_secs(30),
        stop.clone(),
    )
    .await?;
    if diagnostics.cancelled {
        return Ok(());
    }
    if !diagnostics.passed() {
        bail!("active startup doctor checks did not pass; run doctor for details");
    }
    if crate::config::load(&config.path, &context)?.fingerprint != config.fingerprint {
        bail!("configuration changed during active startup checks; restart validation");
    }
    // Private housekeeping is provisioned only after read-only checks succeed.
    use std::os::unix::fs::PermissionsExt;
    let temporary = tempfile::Builder::new()
        .prefix("fridica-parent-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let ssh = crate::exec::ssh::control_directory(context.runtime_dir.as_deref(), context.uid)?;
    if *stop.borrow() {
        return Ok(());
    }
    let result = run(
        config,
        credentials,
        stop,
        Some(composition::Host {
            home: context.home.clone(),
            environment,
            ssh_control_directory: ssh,
            parent_temporary_root: temporary.path().to_owned(),
        }),
        context,
    )
    .await;
    drop(temporary);
    result
}

async fn run(
    config: Config,
    credentials: Credentials,
    stop: watch::Receiver<bool>,
    host: Option<composition::Host>,
    context: crate::config::LoadContext,
) -> Result<()> {
    if *stop.borrow() {
        return Ok(());
    }
    let config = Arc::new(config);
    let store = Store::open(config.state.path.clone()).await?;
    let (finished, following) = watch::channel(false);
    let follower = tokio::spawn(log::follow(store.clone(), config.slack.clone(), following));
    let clock = Arc::new(SystemClock);
    let web = Arc::new(crate::slack::web::SlackClient::from(
        crate::slack::web::client(
            &config,
            store.clone(),
            clock.clone(),
            credentials.user,
            Duration::from_secs(30),
        )?,
    ));
    let mode = if let Some(host) = host {
        composition::Mode::Active(Box::new(composition::Execution::system(
            &config,
            store.clone(),
            clock.clone(),
            host,
        )?))
    } else {
        composition::Mode::ObserveOnly
    };
    let runtime = composition::start(config.clone(), store, web, clock, Arc::new(RandomIds), mode)
        .await?
        .with_configuration_context(context);
    let service = runtime.slack_service(
        credentials.app,
        socket::Options::default(),
        service::Options::default(),
    )?;
    let controls = ControlServer::new(
        config,
        Arc::new(Api::new(service.runtime())),
        Access::OwnerPeer,
        Options::default(),
    );
    let result = service.run_with_lifecycle(stop, controls).await;
    // Let the log print the final shutdown events before returning.
    finished.send_replace(true);
    let _ = follower.await;
    result?;
    Ok(())
}
