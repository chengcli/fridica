//! Fridica's side of the backend drivers: how worker processes launch (local,
//! SSH, confined), where wire traffic is recorded, and how results are parsed.
//! The protocols and per-handle owner task live in `fridica_agent`.
use super::{protocol::*, result};
use crate::{
    config::Config,
    core::{
        delivery::AdapterFuture,
        time::{Clock, Identifiers},
        worker::*,
    },
    exec::{
        local::LocalTransport,
        process::{self, Launch, Process},
        ssh::{LaunchOptions, SshTransport},
    },
    store::Store,
};
use fridica_agent::{Agent, Backend, BoxFuture, Child, LaunchError, OutputFormat, Turn};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, ffi::OsString, io, path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Semaphore;

pub trait Launcher: Send + Sync {
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
}
impl SystemLauncher {
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
        })
    }
}
impl Launcher for SystemLauncher {
    fn validate_config(&self, config: &Config) -> anyhow::Result<()> {
        let current = crate::exec::isolation::Isolation::new(config, &[])?;
        if current != self.isolation {
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
            self.validate_config(&config).map_err(|_| {
                failure(
                    Failure::Refusal,
                    "worker_isolation_configuration_changed",
                    "",
                )
            })?;
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
                .ok_or_else(|| failure(Failure::Refusal, "worker_isolation_target_unknown", ""))?;
            let permit =
                self.probes.clone().acquire_owned().await.map_err(|_| {
                    failure(Failure::Refusal, "worker_isolation_probe_unavailable", "")
                })?;
            let launch = match spec.machine.transport.as_str() {
                "local" => self.isolation.preflight(
                    &LocalTransport {
                        machine: spec.machine.clone(),
                        home: self.home.clone(),
                        excluded_env: spec.excluded_env.clone(),
                    },
                    &base.path,
                    self.environment.clone(),
                ),
                "ssh" => {
                    let mut launch = self.isolation.preflight_remote(
                        &SshTransport {
                            machine: spec.machine.clone(),
                            excluded_env: spec.excluded_env.clone(),
                            control_directory: self.ssh_control_directory.clone(),
                        },
                        &base.path.to_string_lossy(),
                        self.environment.clone(),
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
                        "",
                    ))
                }
            }
            .map_err(|_| failure(Failure::Refusal, "worker_isolation_launch_refused", ""))?;
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
            Err(failure(Failure::Refusal, code, ""))
        })
    }

    fn launch(&self, spec: &WorkerSpec, command: Vec<String>) -> Result<Launch, WorkerFailure> {
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
                        self.environment.clone(),
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
                                self.environment.clone(),
                                &BTreeMap::new(),
                                None,
                                false,
                            )
                        })
                } else {
                    transport.launch(
                        command,
                        &spec.workspace.path,
                        self.environment.clone(),
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
                        self.environment.clone(),
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
                                self.environment.clone(),
                                &BTreeMap::new(),
                                LaunchOptions::default(),
                            )
                        })
                } else {
                    transport.launch(
                        command,
                        &spec.workspace.path.to_string_lossy(),
                        self.environment.clone(),
                        &BTreeMap::new(),
                        LaunchOptions {
                            create: spec.create_cwd(),
                            ..LaunchOptions::default()
                        },
                    )
                }
            }
            _ => return Err(failure(Failure::Refusal, "transport_unavailable", "")),
        };
        launch.map_err(|_| failure(Failure::Refusal, "backend_launch_configuration_failed", ""))
    }
}
#[derive(Clone)]
pub struct Options {
    pub eof_wait: Duration,
    pub close_grace: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            eof_wait: Duration::from_secs(1),
            close_grace: Duration::from_secs(1),
        }
    }
}
/// Records every backend wire event before it takes effect.
pub trait WireRecorder: Send + Sync {
    fn record(&self, context: Value, event: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>>;
}
pub struct DiscardWire;
impl WireRecorder for DiscardWire {
    fn record(&self, _: Value, _: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
    }
}
/// A Fridica recorder as the driver's recorder.
struct Recording(Arc<dyn WireRecorder>);
impl fridica_agent::Recorder for Recording {
    fn record(
        &self,
        context: Value,
        event: Value,
    ) -> BoxFuture<'_, Result<(), fridica_agent::Error>> {
        Box::pin(async move { self.0.record(context, event).await.map_err(agent_error) })
    }
}
// The driver's failure and approval types mirror fridica-core's field for
// field, so conversion is exact both ways.
fn kind(kind: fridica_agent::Failure) -> Failure {
    match kind {
        fridica_agent::Failure::Execution => Failure::Execution,
        fridica_agent::Failure::Refusal => Failure::Refusal,
        fridica_agent::Failure::Cancelled => Failure::Cancelled,
        fridica_agent::Failure::Interrupted => Failure::Interrupted,
        fridica_agent::Failure::RateLimited { retry_at } => Failure::RateLimited { retry_at },
    }
}
fn worker_failure(e: fridica_agent::Error) -> WorkerFailure {
    WorkerFailure {
        kind: kind(e.kind),
        code: e.code,
        backend_session_id: e.backend_session_id,
    }
}
fn agent_error(f: WorkerFailure) -> fridica_agent::Error {
    let kind = match f.kind {
        Failure::Execution => fridica_agent::Failure::Execution,
        Failure::Refusal => fridica_agent::Failure::Refusal,
        Failure::Cancelled => fridica_agent::Failure::Cancelled,
        Failure::Interrupted => fridica_agent::Failure::Interrupted,
        Failure::RateLimited { retry_at } => fridica_agent::Failure::RateLimited { retry_at },
    };
    fridica_agent::Error::new(kind, &f.code, &f.backend_session_id)
}
pub(crate) fn approval_request(r: fridica_agent::ApprovalRequest) -> ApprovalRequest {
    ApprovalRequest {
        kind: r.kind,
        summary: r.summary,
        detail: r.detail,
        backend_request_id: r.backend_request_id,
        cache_key: r.cache_key,
    }
}
pub(crate) fn agent_decision(d: ApprovalDecision) -> fridica_agent::ApprovalDecision {
    match d {
        ApprovalDecision::Once => fridica_agent::ApprovalDecision::Once,
        ApprovalDecision::Session => fridica_agent::ApprovalDecision::Session,
        ApprovalDecision::Deny => fridica_agent::ApprovalDecision::Deny,
    }
}
pub struct StoreWireRecorder {
    pub store: Store,
    pub clock: Arc<dyn Clock>,
}
impl WireRecorder for StoreWireRecorder {
    fn record(&self, context: Value, event: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async move {
            let now = self.clock.now();
            self.store.call(move|c|{
                let tx=c.transaction()?;
                // Received bytes are kept as text when they are UTF-8 (#116).
                let payload=json!({"context":context,"event":crate::store::record::wire(event.clone())}).to_string();
                tx.execute("INSERT INTO replay_events(kind,time,payload_json,complete) VALUES('backend_wire',?,?,?)",rusqlite::params![now,payload,event["incomplete"]!=true])?;
                if event["direction"]=="notice" && event["notice"]["code"]=="claude_permission_mode_fallback" {
                    tx.execute("INSERT INTO health_events(kind,details_json,created) VALUES('claude_permission_mode_fallback',?,?)",rusqlite::params![payload,now])?;
                }
                tx.commit()?;Ok(())
            }).await.map_err(|_|failure(Failure::Execution,"backend_wire_storage_failed",""))
        })
    }
}
pub trait Instructions: Send + Sync {
    fn build(&self, config: &Config, worker: &WorkerRecord) -> anyhow::Result<String>;
}
pub struct BackendFactory {
    pub launcher: Arc<dyn Launcher>,
    pub instructions: Arc<dyn Instructions>,
    pub ids: Arc<dyn Identifiers>,
    pub options: Options,
    pub recorder: Arc<dyn WireRecorder>,
}
impl Factory for BackendFactory {
    fn validate_config(&self, config: &Config) -> anyhow::Result<()> {
        self.launcher.validate_config(config)
    }
    fn admit(
        &self,
        config: Arc<Config>,
        spec: WorkerSpec,
    ) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async move {
            self.validate_config(&config).map_err(|_| {
                failure(
                    Failure::Refusal,
                    "worker_isolation_configuration_changed",
                    "",
                )
            })?;
            self.launcher.admit(config, spec).await
        })
    }

    fn instructions(&self, config: &Config, worker: &WorkerRecord) -> anyhow::Result<String> {
        self.instructions.build(config, worker)
    }
    fn create(&self, spec: WorkerSpec) -> Result<Arc<dyn Worker>, WorkerFailure> {
        Ok(Arc::new(JsonlWorker::new(
            spec,
            self.launcher.clone(),
            self.ids.clone(),
            self.options.clone(),
            self.recorder.clone(),
        )?))
    }
}
fn failure(kind: Failure, code: &str, session: &str) -> WorkerFailure {
    WorkerFailure {
        kind,
        code: code.into(),
        backend_session_id: session.into(),
    }
}
fn seconds(value: f64) -> Duration {
    Duration::try_from_secs_f64(value).unwrap_or(Duration::ZERO)
}
/// The driver settings for a worker; durations that are not positive become
/// zero, which the driver refuses.
pub fn driver_spec(spec: &WorkerSpec, backend: Backend) -> fridica_agent::Spec {
    let p = &spec.workspace.policy;
    fridica_agent::Spec {
        backend,
        instructions: spec.instructions.clone(),
        model: spec.model.clone(),
        reasoning_effort: spec.reasoning_effort.clone(),
        cwd: spec.workspace.path.clone(),
        policy: fridica_agent::Policy {
            mode: p.mode.clone(),
            approvals: p.approvals.clone(),
            network: p.network.clone(),
            claude_prompts: p.claude_prompts.clone(),
        },
        confined: spec.confined(),
        // Scoped fetch must not run beside MCP servers that could reach out.
        forbid_mcp: !p.fetch_repos.is_empty(),
        job_timeout: seconds(spec.job_timeout),
        idle_timeout: seconds(spec.idle_timeout),
        approval_timeout: seconds(p.approval_timeout),
    }
}
fn driver_options(options: &Options) -> fridica_agent::Options {
    fridica_agent::Options {
        // Frozen on the wire: Claude request IDs and Codex client info.
        client: fridica_agent::ClientInfo {
            name: "fridica".into(),
            title: "Fridica".into(),
            version: "2".into(),
        },
        // Workers never get MCP servers: Claude starts with a strict empty MCP
        // configuration, and the Codex startup helper disables every server it
        // finds in the target's settings before app-server initializes.
        disabled_mcp_servers: vec![],
        eof_wait: options.eof_wait,
        close_grace: options.close_grace,
    }
}
/// A started worker process, stopped with its process group on drop.
struct ProcessChild(Process);
impl Child for ProcessChild {
    fn take_stdin(&mut self) -> Option<fridica_agent::Stdin> {
        self.0.stdin().map(|s| Box::new(s) as fridica_agent::Stdin)
    }
    fn take_stdout(&mut self) -> Option<fridica_agent::Output> {
        self.0
            .stdout()
            .map(|s| Box::new(s) as fridica_agent::Output)
    }
    fn take_stderr(&mut self) -> Option<fridica_agent::Output> {
        self.0
            .stderr()
            .map(|s| Box::new(s) as fridica_agent::Output)
    }
    fn try_exit(&mut self) -> io::Result<Option<i32>> {
        Ok(self
            .0
            .try_wait()
            .map_err(io::Error::other)?
            .map(process::returncode))
    }
    fn eof_status(&mut self, wait: Duration) -> BoxFuture<'_, io::Result<Option<i32>>> {
        Box::pin(async move {
            Ok(self
                .0
                .eof_status(wait)
                .await
                .map_err(io::Error::other)?
                .map(process::returncode))
        })
    }
    fn terminate(&mut self, grace: Duration) -> BoxFuture<'_, io::Result<()>> {
        Box::pin(async move { self.0.terminate(grace).await.map_err(io::Error::other) })
    }
    fn refusal(&self, status: Option<i32>, stderr: &[u8]) -> Option<String> {
        // The worker MCP startup wrapper's own refusal.
        (status == Some(97) && stderr == b"fridica worker MCP: setup refused\n")
            .then(|| "worker_mcp_settings_refused".into())
    }
}
/// Launches one worker's backend through Fridica's transport and isolation.
struct WorkerLauncher {
    spec: WorkerSpec,
    launcher: Arc<dyn Launcher>,
}
impl fridica_agent::Launcher for WorkerLauncher {
    fn launch(&self, command: Vec<String>) -> Result<Box<dyn Child>, LaunchError> {
        let launch = self
            .launcher
            .launch(&self.spec, command)
            .map_err(|f| LaunchError::Refused(agent_error(f)))?;
        let process = Process::start(&launch).map_err(|_| LaunchError::StartFailed)?;
        Ok(Box::new(ProcessChild(process)))
    }
}
/// Deterministic Claude session UUIDs from the runtime's identifier source.
struct SessionUuids(Arc<dyn Identifiers>);
impl fridica_agent::SessionIds for SessionUuids {
    fn new_session_id(&self) -> String {
        let hash = Sha256::digest(self.0.next("backend_session").as_bytes());
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&hash[..16]);
        bytes[6] = (bytes[6] & 15) | 64;
        bytes[8] = (bytes[8] & 63) | 128;
        uuid::Uuid::from_bytes(bytes).to_string()
    }
}
/// Approvals for one job go to the owner broker with that worker and job.
struct JobApprover {
    approvals: Arc<dyn ApprovalHandler>,
    worker: WorkerRecord,
    job: Job,
}
impl fridica_agent::Approver for JobApprover {
    fn request(
        &self,
        request: fridica_agent::ApprovalRequest,
    ) -> BoxFuture<'_, fridica_agent::ApprovalDecision> {
        Box::pin(async move {
            let decision = self
                .approvals
                .request(
                    self.worker.clone(),
                    self.job.clone(),
                    approval_request(request),
                )
                .await;
            agent_decision(decision)
        })
    }
}
fn worker_result() -> OutputFormat {
    OutputFormat {
        schema: result::schema(),
        note: result::FORMAT_NOTE.into(),
        repair: result::SUMMARIZE_PROMPT.into(),
        accept: Arc::new(|text| result::parse(text).is_some()),
    }
}
pub struct JsonlWorker {
    agent: Agent,
    backend: String,
}
impl JsonlWorker {
    pub fn new(
        spec: WorkerSpec,
        launcher: Arc<dyn Launcher>,
        ids: Arc<dyn Identifiers>,
        options: Options,
        recorder: Arc<dyn WireRecorder>,
    ) -> Result<Self, WorkerFailure> {
        let backend = match spec.backend.as_str() {
            "claude" => Some(Backend::Claude),
            "codex" => Some(Backend::Codex),
            _ => None,
        };
        let Some(backend) = backend.filter(|_| {
            [
                spec.job_timeout,
                spec.idle_timeout,
                spec.workspace.policy.approval_timeout,
            ]
            .iter()
            .all(|v| *v > 0. && Duration::try_from_secs_f64(*v).is_ok())
        }) else {
            return Err(failure(
                Failure::Refusal,
                "invalid_backend_configuration",
                "",
            ));
        };
        let agent = Agent::new(
            driver_spec(&spec, backend),
            driver_options(&options),
            Arc::new(WorkerLauncher {
                spec: spec.clone(),
                launcher,
            }),
            Arc::new(SessionUuids(ids)),
            Arc::new(Recording(recorder)),
        )
        .map_err(worker_failure)?;
        Ok(Self {
            agent,
            backend: spec.backend,
        })
    }
}
impl Worker for JsonlWorker {
    fn alive(&self) -> bool {
        self.agent.alive()
    }
    fn busy(&self) -> bool {
        self.agent.busy()
    }
    fn run(
        &self,
        request: RunRequest,
        worker: WorkerRecord,
        job: Job,
        approvals: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>> {
        Box::pin(async move {
            let context = json!({"worker_id":worker.id,"job_id":job.id,"attempt":request.attempt,"backend":self.backend});
            let reply = self
                .agent
                .run(Turn {
                    prompt: request.brief,
                    resume: request.resume,
                    fork_from: request.fork_from,
                    context,
                    approver: Arc::new(JobApprover {
                        approvals,
                        worker,
                        job,
                    }),
                    output: Some(worker_result()),
                })
                .await
                .map_err(worker_failure)?;
            Ok(Outcome {
                result: result::parse(&reply.text).unwrap_or_else(|| result::fallback(&reply.text)),
                backend_session_id: reply.backend_session_id,
            })
        })
    }
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { self.agent.interrupt().await.map_err(worker_failure) })
    }
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { self.agent.close().await.map_err(worker_failure) })
    }
}
