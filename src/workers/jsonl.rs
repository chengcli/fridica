//! One owner task per backend handle. Caller cancellation and close requests
//! cannot strand a locked protocol reader or a worker waiting for approval.
use super::{claude, codex, protocol::*, result};
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
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    ffi::OsString,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::ChildStdin,
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

pub trait Launcher: Send + Sync {
    fn launch(&self, spec: &WorkerSpec, command: Vec<String>) -> Result<Launch, WorkerFailure>;
}
/// Explicit owner environment snapshot; no process-global environment mutation.
pub struct SystemLauncher {
    pub home: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub ssh_control_directory: PathBuf,
    pub isolation: crate::exec::isolation::Isolation,
}
impl Launcher for SystemLauncher {
    fn launch(&self, spec: &WorkerSpec, command: Vec<String>) -> Result<Launch, WorkerFailure> {
        let roots = vec![spec.workspace.path.to_string_lossy().into_owned()];
        let confine = spec.confined().then_some(roots.as_slice());
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
            "ssh" => SshTransport {
                machine: spec.machine.clone(),
                excluded_env: spec.excluded_env.clone(),
                control_directory: self.ssh_control_directory.clone(),
            }
            .launch(
                command,
                &spec.workspace.path.to_string_lossy(),
                self.environment.clone(),
                &BTreeMap::new(),
                LaunchOptions {
                    confine,
                    create: spec.create_cwd(),
                    timeout: None,
                },
            ),
            _ => return Err(failure(Failure::Refusal, "transport_unavailable", "")),
        };
        launch.map_err(|_| failure(Failure::Refusal, "backend_launch_configuration_failed", ""))
    }
}
#[derive(Clone)]
pub struct Options {
    /// Every configured alias for Fridica MCP must be supplied by daemon wiring.
    /// Overrides are added before app-server starts, never after MCP initialization.
    pub disabled_mcp_servers: Vec<String>,
    pub eof_wait: Duration,
    pub close_grace: Duration,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            disabled_mcp_servers: vec![],
            eof_wait: Duration::from_secs(1),
            close_grace: Duration::from_secs(1),
        }
    }
}
pub trait WireRecorder: Send + Sync {
    fn record(&self, context: Value, event: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>>;
}
pub struct DiscardWire;
impl WireRecorder for DiscardWire {
    fn record(&self, _: Value, _: Value) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async { Ok(()) })
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
                let payload=json!({"context":context,"event":event}).to_string();
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
#[derive(Debug)]
pub(super) struct WireError {
    pub kind: Failure,
    pub code: String,
    pub lost: bool,
}
impl WireError {
    pub fn execution(code: &str) -> Self {
        Self {
            kind: Failure::Execution,
            code: code.into(),
            lost: false,
        }
    }
    pub fn refusal(code: &str) -> Self {
        Self {
            kind: Failure::Refusal,
            code: code.into(),
            lost: false,
        }
    }
    pub fn interrupted() -> Self {
        Self {
            kind: Failure::Interrupted,
            code: "backend_interrupted".into(),
            lost: false,
        }
    }
    fn closed() -> Self {
        Self {
            kind: Failure::Cancelled,
            code: "backend_closed".into(),
            lost: false,
        }
    }
    pub fn lost() -> Self {
        Self {
            kind: Failure::Execution,
            code: "backend_session_unavailable".into(),
            lost: true,
        }
    }
}
impl From<WorkerFailure> for WireError {
    fn from(f: WorkerFailure) -> Self {
        Self {
            kind: f.kind,
            code: f.code,
            lost: false,
        }
    }
}
#[derive(Clone, Copy)]
enum Control {
    Interrupt,
    Close,
}
struct Run {
    request: RunRequest,
    worker: WorkerRecord,
    job: Job,
    approvals: Arc<dyn ApprovalHandler>,
    control: watch::Receiver<Control>,
    reply: oneshot::Sender<Result<Outcome, WorkerFailure>>,
}
enum Command {
    Run(Box<Run>),
    Close(oneshot::Sender<Result<(), WorkerFailure>>),
}
pub struct JsonlWorker {
    commands: mpsc::Sender<Command>,
    control: watch::Sender<Control>,
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}
impl JsonlWorker {
    pub fn new(
        spec: WorkerSpec,
        launcher: Arc<dyn Launcher>,
        ids: Arc<dyn Identifiers>,
        options: Options,
        recorder: Arc<dyn WireRecorder>,
    ) -> Result<Self, WorkerFailure> {
        if !["codex", "claude"].contains(&spec.backend.as_str())
            || [
                spec.job_timeout,
                spec.idle_timeout,
                spec.workspace.policy.approval_timeout,
            ]
            .iter()
            .any(|v| *v <= 0. || Duration::try_from_secs_f64(*v).is_err())
            || options.eof_wait.is_zero()
            || options.close_grace.is_zero()
            || options
                .disabled_mcp_servers
                .iter()
                .any(|s| s.is_empty() || s.contains('\0'))
        {
            return Err(failure(
                Failure::Refusal,
                "invalid_backend_configuration",
                "",
            ));
        }
        let (commands, receiver) = mpsc::channel(2);
        let (control, _) = watch::channel(Control::Interrupt);
        let alive = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        tokio::spawn(owner(
            Owner {
                spec,
                launcher,
                ids,
                options,
                recorder,
                alive: alive.clone(),
                busy: busy.clone(),
            },
            receiver,
        ));
        Ok(Self {
            commands,
            control,
            alive,
            busy,
        })
    }
}
impl Worker for JsonlWorker {
    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
    fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
    fn run(
        &self,
        request: RunRequest,
        worker: WorkerRecord,
        job: Job,
        approvals: Arc<dyn ApprovalHandler>,
    ) -> AdapterFuture<'_, Result<Outcome, WorkerFailure>> {
        Box::pin(async move {
            if self
                .busy
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                return Err(failure(Failure::Refusal, "backend_busy", ""));
            }
            let (reply, receive) = oneshot::channel();
            let command = Command::Run(Box::new(Run {
                request,
                worker,
                job,
                approvals,
                control: self.control.subscribe(),
                reply,
            }));
            if self.commands.try_send(command).is_err() {
                self.busy.store(false, Ordering::SeqCst);
                return Err(failure(Failure::Execution, "backend_owner_unavailable", ""));
            }
            receive
                .await
                .unwrap_or_else(|_| Err(failure(Failure::Execution, "backend_owner_stopped", "")))
        })
    }
    fn interrupt(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async {
            if self.busy() {
                self.control.send_replace(Control::Interrupt);
            }
            Ok(())
        })
    }
    fn close(&self) -> AdapterFuture<'_, Result<(), WorkerFailure>> {
        Box::pin(async {
            self.control.send_replace(Control::Close);
            let (reply, receive) = oneshot::channel();
            self.commands
                .send(Command::Close(reply))
                .await
                .map_err(|_| failure(Failure::Execution, "backend_owner_unavailable", ""))?;
            receive
                .await
                .unwrap_or_else(|_| Err(failure(Failure::Execution, "backend_owner_stopped", "")))
        })
    }
}
struct Owner {
    spec: WorkerSpec,
    launcher: Arc<dyn Launcher>,
    ids: Arc<dyn Identifiers>,
    options: Options,
    recorder: Arc<dyn WireRecorder>,
    alive: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
}
async fn cleanup(session: &mut Option<Session>, owner: &Owner) -> Result<(), WorkerFailure> {
    if let Some(s) = session {
        s.stdin.take();
        s.tainted = true;
        s.process
            .terminate(owner.options.close_grace)
            .await
            .map_err(|_| failure(Failure::Execution, "backend_close_unconfirmed", &s.session))?;
    }
    session.take();
    owner.alive.store(false, Ordering::SeqCst);
    Ok(())
}
async fn owner(owner: Owner, mut commands: mpsc::Receiver<Command>) {
    let mut session = None;
    loop {
        let command = tokio::select! {biased;
            command=commands.recv()=>command,
            _=tokio::time::sleep(Duration::from_secs_f64(owner.spec.idle_timeout)),if session.is_some()=>{let _=cleanup(&mut session,&owner).await;continue;},
        };
        match command {
            Some(Command::Run(run)) => {
                let Run {
                    request,
                    worker,
                    job,
                    approvals,
                    control,
                    mut reply,
                } = *run;
                let mut closing = control.clone();
                let result = {
                    let operation = run_job(
                        &owner,
                        &mut session,
                        request,
                        worker,
                        job,
                        approvals,
                        control,
                    );
                    tokio::select! {biased;
                        _=reply.closed()=>Err(failure(Failure::Cancelled,"backend_caller_cancelled","")),
                        _=async { loop {
                            if closing.changed().await.is_err() || matches!(*closing.borrow_and_update(),Control::Close) { break; }
                        }}=>Err(failure(Failure::Cancelled,"backend_closed","")),
                        result=tokio::time::timeout(Duration::from_secs_f64(owner.spec.job_timeout),operation)=>result.unwrap_or_else(|_|Err(failure(Failure::Execution,"backend_job_timeout",""))),
                    }
                };
                let result = result.map_err(|mut error| {
                    if error.backend_session_id.is_empty() {
                        if let Some(s) = &session {
                            error.backend_session_id = s.session.clone();
                        }
                    }
                    error
                });
                let result = if result.is_err() {
                    match cleanup(&mut session, &owner).await {
                        Ok(()) => result,
                        Err(error) => Err(error),
                    }
                } else {
                    result
                };
                owner.busy.store(false, Ordering::SeqCst);
                let _ = reply.send(result);
            }
            Some(Command::Close(reply)) => {
                let _ = reply.send(cleanup(&mut session, &owner).await);
            }
            None => {
                let _ = cleanup(&mut session, &owner).await;
                break;
            }
        }
    }
}
fn session_uuid(ids: &dyn Identifiers) -> String {
    let hash = Sha256::digest(ids.next("backend_session").as_bytes());
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&hash[..16]);
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    uuid::Uuid::from_bytes(bytes).to_string()
}
#[allow(clippy::too_many_arguments)]
async fn run_job(
    owner: &Owner,
    slot: &mut Option<Session>,
    request: RunRequest,
    worker: WorkerRecord,
    job: Job,
    approvals: Arc<dyn ApprovalHandler>,
    mut control: watch::Receiver<Control>,
) -> Result<Outcome, WorkerFailure> {
    if control.has_changed().unwrap_or(true) {
        return Err(failure(
            Failure::Interrupted,
            "backend_interrupted_before_start",
            "",
        ));
    }
    control.borrow_and_update();
    let context = json!({"worker_id":worker.id,"job_id":job.id,"attempt":request.attempt,"backend":owner.spec.backend});
    let replace = if let Some(s) = slot {
        s.tainted
            || s.process
                .try_wait()
                .map_err(|_| failure(Failure::Execution, "backend_wait_failed", &s.session))?
                .is_some()
            || (!request.resume.is_empty() && s.session != request.resume)
    } else {
        false
    };
    if replace {
        cleanup(slot, owner).await?;
    }
    let prompt = if owner.spec.backend == "claude" {
        format!("{}\n\n{}", request.brief, result::FORMAT_NOTE)
    } else {
        request.brief.clone()
    };
    let mut resume = request.resume.clone();
    for fresh_attempt in 0..2 {
        if slot.is_none() {
            let id = if owner.spec.backend == "claude" && resume.is_empty() {
                session_uuid(owner.ids.as_ref())
            } else {
                resume.clone()
            };
            let spec = &owner.spec;
            let command = if spec.backend == "codex" {
                codex::command(spec, &owner.options.disabled_mcp_servers)
            } else {
                claude::command(spec, &resume, &id)
            };
            owner
                .recorder
                .record(
                    context.clone(),
                    json!({"direction":"start","command":command,"resume":resume}),
                )
                .await?;
            let launch = owner.launcher.launch(spec, command)?;
            *slot = Some(Session::start(
                owner,
                launch,
                id,
                resume.clone(),
                Call {
                    worker: worker.clone(),
                    job: job.clone(),
                    approvals: approvals.clone(),
                    control: control.clone(),
                    context: context.clone(),
                },
            )?);
            owner.alive.store(true, Ordering::SeqCst);
        }
        let s = slot.as_mut().unwrap();
        s.call = Call {
            worker: worker.clone(),
            job: job.clone(),
            approvals: approvals.clone(),
            control: control.clone(),
            context: context.clone(),
        };
        s.interrupted = false;
        s.turn.clear();
        let operation: Result<WorkerResult, WireError> = async {
            if !s.initialized {
                match s.spec.backend.as_str() {
                    "codex" => codex::handshake(s).await?,
                    _ => claude::handshake(s).await?,
                };
                s.initialized = true;
            }
            let text = s.job(&prompt).await?;
            if let Some(result) = result::parse(&text) {
                return Ok(result);
            }
            if !s.interrupted {
                if let Ok(summary) = s.job(result::SUMMARIZE_PROMPT).await {
                    if let Some(result) = result::parse(&summary) {
                        return Ok(result);
                    }
                }
            }
            Ok(result::fallback(&text))
        }
        .await;
        let id = s.session.clone();
        match operation {
            Ok(result) => {
                return Ok(Outcome {
                    result,
                    backend_session_id: id,
                })
            }
            Err(e) if e.lost && fresh_attempt == 0 && !resume.is_empty() => {
                control = s.call.control.clone();
                cleanup(slot, owner).await?;
                resume.clear();
            }
            Err(e) => return Err(failure(e.kind, &e.code, &id)),
        }
    }
    unreachable!()
}
struct Call {
    worker: WorkerRecord,
    job: Job,
    approvals: Arc<dyn ApprovalHandler>,
    control: watch::Receiver<Control>,
    context: Value,
}
enum Output {
    Line(Vec<u8>),
    Error,
    Eof,
}
pub(super) struct Session {
    pub spec: WorkerSpec,
    pub session: String,
    pub resume: String,
    pub turn: String,
    pub in_turn: bool,
    pub interrupted: bool,
    process: Process,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<Output>,
    reader: JoinHandle<()>,
    stderr_task: JoinHandle<()>,
    stderr: Arc<StdMutex<Vec<u8>>>,
    call: Call,
    recorder: Arc<dyn WireRecorder>,
    options: Options,
    sequence: u64,
    initialized: bool,
    tainted: bool,
    pending: VecDeque<Value>,
    approved: HashSet<String>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.reader.abort();
        self.stderr_task.abort();
    }
}
impl Session {
    fn start(
        owner: &Owner,
        launch: Launch,
        session: String,
        resume: String,
        call: Call,
    ) -> Result<Self, WorkerFailure> {
        let mut process = Process::start(&launch)
            .map_err(|_| failure(Failure::Execution, "backend_start_failed", &session))?;
        let stdin = process.stdin();
        let stdout = process.stdout().unwrap();
        let mut stderr = process.stderr().unwrap();
        let (send, lines) = mpsc::channel(16);
        let reader = tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = vec![];
                loop {
                    let chunk = match reader.fill_buf().await {
                        Ok(b) => b,
                        Err(_) => {
                            let _ = send.send(Output::Error).await;
                            return;
                        }
                    };
                    if chunk.is_empty() {
                        if !line.is_empty() {
                            let _ = send.send(Output::Line(line)).await;
                        }
                        let _ = send.send(Output::Eof).await;
                        return;
                    }
                    let size = chunk
                        .iter()
                        .position(|b| *b == b'\n')
                        .map_or(chunk.len(), |p| p + 1);
                    let ended = chunk[size - 1] == b'\n';
                    if line.len() + size > process::OUTPUT_LIMIT {
                        let _ = send.send(Output::Error).await;
                        return;
                    }
                    line.extend_from_slice(&chunk[..size]);
                    reader.consume(size);
                    if ended {
                        break;
                    }
                }
                if send.send(Output::Line(line)).await.is_err() {
                    return;
                }
            }
        });
        let buffer = Arc::new(StdMutex::new(Vec::new()));
        let tail = buffer.clone();
        let stderr_task = tokio::spawn(async move {
            let mut chunk = [0; 4096];
            while let Ok(n) = stderr.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&chunk[..n]);
                let excess = tail.len().saturating_sub(64 * 1024);
                tail.drain(..excess);
            }
        });
        Ok(Self {
            spec: owner.spec.clone(),
            session,
            resume,
            turn: String::new(),
            in_turn: false,
            interrupted: false,
            process,
            stdin,
            lines,
            reader,
            stderr_task,
            stderr: buffer,
            call,
            recorder: owner.recorder.clone(),
            options: owner.options.clone(),
            sequence: 0,
            initialized: false,
            tainted: false,
            pending: VecDeque::new(),
            approved: HashSet::new(),
        })
    }
    pub fn next_id(&mut self) -> u64 {
        self.sequence += 1;
        self.sequence
    }
    pub async fn notice(&self, notice: Value) -> Result<(), WireError> {
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"notice","notice":notice}),
            )
            .await?;
        Ok(())
    }
    pub async fn send(&mut self, message: Value) -> Result<(), WireError> {
        let mut bytes = serde_json::to_vec(&message)
            .map_err(|_| WireError::refusal("backend_message_invalid"))?;
        bytes.push(b'\n');
        if bytes.len() > process::OUTPUT_LIMIT {
            return Err(WireError::refusal("backend_message_too_large"));
        }
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"send_intent","message":message}),
            )
            .await?;
        self.stdin
            .as_mut()
            .ok_or_else(WireError::closed)?
            .write_all(&bytes)
            .await
            .map_err(|_| WireError::execution("backend_write_failed"))?;
        self.recorder
            .record(
                self.call.context.clone(),
                json!({"direction":"write_completed","message":message}),
            )
            .await?;
        Ok(())
    }
    async fn on_control(&mut self) -> Result<(), WireError> {
        let control = *self.call.control.borrow_and_update();
        match control {
            Control::Close => Err(WireError::closed()),
            Control::Interrupt => {
                self.interrupted = true;
                self.send_interrupt().await
            }
        }
    }
    pub async fn send_interrupt(&mut self) -> Result<(), WireError> {
        if self.spec.backend == "codex" {
            if self.turn.is_empty() {
                return Ok(());
            }
            let id = self.next_id();
            self.send(json!({"id":id,"method":"turn/interrupt","params":{"threadId":self.session,"turnId":self.turn}})).await
        } else {
            if !self.in_turn {
                return Ok(());
            }
            let id = self.next_id();
            self.send(json!({"type":"control_request","request_id":format!("fridica-{id}"),"request":{"subtype":"interrupt"}})).await
        }
    }
    pub async fn receive(&mut self, wire_only: bool) -> Result<Value, WireError> {
        loop {
            if !wire_only {
                if let Some(m) = self.pending.pop_front() {
                    return Ok(m);
                }
            }
            let output = tokio::select! {biased;
                changed=self.call.control.changed()=>{changed.map_err(|_|WireError::closed())?;self.on_control().await?;continue;},
                output=self.lines.recv()=>output,
            };
            match output {
                Some(Output::Line(line)) => {
                    self.recorder
                        .record(
                            self.call.context.clone(),
                            json!({"direction":"received","bytes":line}),
                        )
                        .await?;
                    if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                        if message.is_object() {
                            return Ok(message);
                        }
                    }
                }
                Some(Output::Error) => {
                    self.recorder.record(self.call.context.clone(),json!({"direction":"read_failed","incomplete":true,"code":"backend_output_invalid_or_too_large"})).await?;
                    return Err(WireError::execution("backend_output_invalid_or_too_large"));
                }
                _ => {
                    let deadline = tokio::time::Instant::now() + self.options.eof_wait;
                    let status = self
                        .process
                        .eof_status(self.options.eof_wait)
                        .await
                        .map_err(|_| WireError::execution("backend_wait_failed"))?;
                    let _ = tokio::time::timeout_at(deadline, &mut self.stderr_task).await;
                    let stderr = self.stderr.lock().unwrap().clone();
                    self.recorder.record(self.call.context.clone(),json!({"direction":"eof","status":status.map(process::returncode),"stderr_tail":stderr,"incomplete":stderr.len()>=64*1024})).await?;
                    let detail = String::from_utf8_lossy(&stderr);
                    if detail.contains("No conversation found with session ID")
                        || detail.contains("no rollout found for thread id")
                    {
                        return Err(WireError::lost());
                    }
                    return Err(WireError::execution(
                        &status
                            .map(|s| format!("backend_exit_{}", process::returncode(s)))
                            .unwrap_or_else(|| "backend_output_closed_before_exit".into()),
                    ));
                }
            }
        }
    }
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, WireError> {
        let id = self.next_id();
        self.send(json!({"id":id,"method":method,"params":params}))
            .await?;
        loop {
            let m = self.receive(true).await?;
            if m["id"] == id && m.get("method").is_none() {
                if m.get("error").is_some() {
                    if method == "thread/resume"
                        && m["error"]["message"]
                            .as_str()
                            .is_some_and(|s| s.contains("no rollout found for thread id"))
                    {
                        return Err(WireError::lost());
                    }
                    if matches!(m["error"]["code"].as_i64(), Some(-32602..=-32600)) {
                        return Err(WireError::refusal("codex_request_rejected"));
                    }
                    return Err(WireError::execution("codex_request_failed"));
                }
                return Ok(if m["result"].is_object() {
                    m["result"].clone()
                } else {
                    json!({})
                });
            }
            if m.get("id").is_some() {
                codex::dispatch(self, m).await?;
            } else if matches!(
                m["method"].as_str(),
                Some("item/completed" | "turn/completed" | "error")
            ) {
                if self.pending.len() >= 64 {
                    return Err(WireError::execution("backend_pending_output_limit"));
                }
                self.pending.push_back(m);
            }
        }
    }
    pub async fn approve(
        &mut self,
        request: ApprovalRequest,
    ) -> Result<ApprovalDecision, WireError> {
        if self.interrupted
            || self.spec.workspace.policy.approvals == "never"
            || (self.spec.backend == "claude"
                && self.spec.workspace.policy.claude_prompts == "none")
        {
            return Ok(ApprovalDecision::Deny);
        }
        if !request.cache_key.is_empty() && self.approved.contains(&request.cache_key) {
            return Ok(ApprovalDecision::Session);
        }
        let approvals = self.call.approvals.clone();
        let pending = approvals.request(
            self.call.worker.clone(),
            self.call.job.clone(),
            request.clone(),
        );
        let decision = tokio::select! {biased;
            changed=self.call.control.changed()=>{changed.map_err(|_|WireError::closed())?;self.on_control().await?;ApprovalDecision::Deny},
            _=tokio::time::sleep(Duration::from_secs_f64(self.spec.workspace.policy.approval_timeout))=>ApprovalDecision::Deny,
            decision=pending=>decision,
        };
        if decision == ApprovalDecision::Session && !request.cache_key.is_empty() {
            self.approved.insert(request.cache_key);
        }
        Ok(decision)
    }
    async fn job(&mut self, prompt: &str) -> Result<String, WireError> {
        match self.spec.backend.as_str() {
            "codex" => codex::job(self, prompt).await,
            _ => claude::job(self, prompt).await,
        }
    }
}
