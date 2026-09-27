Appendix: skeleton code
=======================

The listings below are skeletons: signatures, types and the control flow that the sections above
refer to. Bodies are ``todo!()`` or a comment. They compile in spirit, not yet in fact; their
purpose is to fix names and shapes before implementation starts.

Package manifest
----------------

.. code-block:: toml

   # Cargo.toml — one package, one library, two binaries
   [package]
   name = "fridica"
   version = "0.4.0"
   edition = "2021"
   rust-version = "1.80"
   license = "MIT"

   [lib]
   name = "fridica"
   path = "src/lib.rs"

   [[bin]]
   name = "fridica"
   path = "src/bin/fridica.rs"

   [[bin]]
   name = "fridica-overseer"
   path = "src/bin/fridica-overseer.rs"

   [dependencies]
   tokio = { version = "1", features = ["rt-multi-thread", "macros", "process", "net",
                                        "sync", "time", "signal", "io-util"] }
   tokio-util = "0.7"
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   schemars = "0.8"
   rusqlite = { version = "0.32", features = ["bundled", "serde_json"] }
   slack-morphism = { version = "2", features = ["hyper", "socket-mode"] }
   reqwest = { version = "0.12", default-features = false,
               features = ["rustls-tls", "json", "stream"] }
   octocrab = "0.41"
   toml = "0.8"
   toml_edit = "0.22"
   clap = { version = "4", features = ["derive"] }
   tracing = "0.1"
   tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
   thiserror = "1"
   anyhow = "1"
   nix = { version = "0.29", features = ["signal", "process", "fs"] }
   axum = "0.7"
   hyper = "1"
   hyperlocal = "0.9"
   rmcp = { version = "0.2", features = ["server", "transport-io"] }
   include_dir = "0.7"
   uuid = { version = "1", features = ["v4", "serde"] }
   sha2 = "0.10"
   chrono = { version = "0.4", features = ["serde"] }
   chrono-tz = "0.9"

   [dev-dependencies]
   syn = { version = "2", features = ["full", "visit"] }   # the layering test parses `use` items

.. code-block:: rust

   // src/lib.rs — the module list is the architecture; nothing else lives here
   pub mod core;
   pub mod config;
   pub mod store;
   pub mod slack;
   pub mod github;
   pub mod machines;
   pub mod threads;
   pub mod parent;
   pub mod workers;
   pub mod exec;
   pub mod approvals;
   pub mod attention;
   pub mod overseer;
   pub mod report;
   pub mod control;
   pub mod mcp;
   pub mod dashboard;
   pub mod doctor;
   pub mod cli;
   pub mod app;      // Daemon, the composition root, as in 0.3

.. code-block:: rust

   // tests/layers.rs — dependencies point downward only (interfaces → coordination → execution
   // → foundation); a `use crate::x` from a lower layer into a higher one fails this test
   const LAYERS: &[(&str, &[&str])] = &[
       ("interfaces",   &["slack", "control", "dashboard", "cli", "doctor", "mcp", "report", "app"]),
       ("coordination", &["threads", "parent", "attention", "overseer", "github"]),
       ("execution",    &["workers", "exec", "approvals", "machines"]),
       ("foundation",   &["core", "config", "store"]),
   ];

   #[test]
   fn modules_depend_downward_only() {
       for (module, uses) in crate_uses("src") {           // (top-level module, modules it `use`s)
           let from = layer_of(module);
           for used in uses {
               assert!(layer_of(used) >= from,             // higher index = lower layer
                       "{module} ({}) must not use {used} ({})", LAYERS[from].0, LAYERS[layer_of(used)].0);
           }
       }
   }

Core: identifiers and models
----------------------------

.. code-block:: rust

   // src/core/ids.rs
   use serde::{Deserialize, Serialize};

   macro_rules! string_id {
       ($name:ident) => {
           #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
           #[serde(transparent)]
           pub struct $name(pub String);
           impl std::fmt::Display for $name {
               fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                   f.write_str(&self.0)
               }
           }
       };
   }
   string_id!(WorkspaceId);
   string_id!(ChannelId);
   string_id!(SlackTs);
   string_id!(EventId);
   string_id!(SlackUserId);
   string_id!(MachineName);
   string_id!(WorkspaceName);
   string_id!(BackendSessionId);
   string_id!(WorkerId);   // 0.3 ids are short hex strings; kept as-is (§4)
   string_id!(JobId);
   string_id!(RepoRef);    // "owner/name"

   #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
   pub struct ThreadId {
       pub workspace: WorkspaceId,
       pub channel: ChannelId,
       pub root_ts: SlackTs,
   }
   impl ThreadId {
       /// "{workspace}:{channel}:{root_ts}", the 0.3 form
       pub fn parse(s: &str) -> Option<Self> { todo!() }
   }
   impl std::fmt::Display for ThreadId {
       fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
           write!(f, "{}:{}:{}", self.workspace, self.channel, self.root_ts)
       }
   }

   macro_rules! uuid_id {
       ($name:ident) => {
           #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
           #[serde(transparent)]
           pub struct $name(pub uuid::Uuid);
           impl $name { pub fn new() -> Self { Self(uuid::Uuid::new_v4()) } }
       };
   }
   uuid_id!(ObligationId);
   uuid_id!(WorkItemId);
   uuid_id!(OverseerRequestId);
   uuid_id!(ActionId);

   #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
   #[serde(transparent)]
   pub struct OutboxId(pub i64);
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
   #[serde(transparent)]
   pub struct InboxId(pub i64);

.. code-block:: rust

   // src/core/models.rs
   use super::ids::*;

   #[derive(Clone, Debug, Serialize, Deserialize)]
   pub struct FridicaMeta {
       pub owner: SlackUserId,
       pub session: String,
       pub turn: u32,
       pub status: String,
       pub kind: MetaKind,
       pub worker: String,
       pub v: u8,
   }
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum MetaKind { Reply, Report, Notice, Upload, DebriefRoot, Overseer, DailyReport }

   #[derive(Clone, Debug)]
   pub struct Message {
       pub event: EventId,
       pub workspace: WorkspaceId,
       pub channel: ChannelId,
       pub ts: SlackTs,
       pub thread_ts: Option<SlackTs>,
       pub sender: SlackUserId,
       pub text: String,
       pub files: Vec<FileRef>,
       pub attachments: Vec<Attachment>,   // F3
       pub source: Source,
       pub meta: Option<FridicaMeta>,
       pub received_at: f64,
   }
   impl Message {
       pub fn root_ts(&self) -> &SlackTs { self.thread_ts.as_ref().unwrap_or(&self.ts) }
       pub fn thread(&self) -> ThreadId {
           ThreadId {
               workspace: self.workspace.clone(),
               channel: self.channel.clone(),
               root_ts: self.root_ts().clone(),
           }
       }
       pub fn generated(&self) -> bool { self.meta.is_some() }
       pub fn mentions(&self, user: &SlackUserId) -> bool {
           self.text.contains(&format!("<@{}>", user))
       }
   }
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum Source { Socket, Catchup, SelfPost }

   #[derive(Clone, Debug, Serialize, Deserialize)]
   pub enum ThreadControl {
       Active,
       Paused { by: Actor, reason: String, since: f64 },   // never set by a rule
       Closed,
       Archived,
       Cleaned,
   }
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum ThreadStatus { New, Complete, Waiting, Blocked, Working }

   #[derive(Clone, Debug, Default, Serialize, Deserialize)]
   pub struct StickyContext {
       pub machine: String,
       pub workspace: String,
       pub repo: String,
       pub branch: String,
       pub backend: String,
   }

   #[derive(Clone, Debug)]
   pub struct ThreadSession {
       pub id: ThreadId,
       pub status: ThreadStatus,
       pub control: ThreadControl,
       pub turns: u32,
       pub wait_streak: u32,           // a signal, not a gate (§5)
       pub quiet_streak: u32,          // 0.3's no_progress, same column
       pub throttled_until: Option<f64>,   // new: §5
       pub last_reply_hash: String,
       pub reset_at: Option<f64>,
       pub summary: String,
       pub decisions: Vec<String>,
       pub context: StickyContext,
       pub debriefed_turn: u32,
       pub last_unsolicited: f64,
       pub created: f64,
       pub updated: f64,
       pub version: u64,
   }
   impl ThreadSession {
       pub fn waiting(&self) -> bool { self.status == ThreadStatus::Waiting }
   }

   // WorkerResult, Change, Validation, ArtifactRef, MachineState: the 0.3 fields, serde snake_case

   #[derive(Clone, Debug, Serialize, Deserialize)]
   #[serde(tag = "kind", rename_all = "snake_case")]
   pub enum InboxItem {
       Message { event: EventId },
       WorkerResult { job: JobId },
       WorkerInterrupted { job: JobId, cause: InterruptCause },
       Control { action: ControlAction, actor: Actor },
       OwnerInstruction { text: String },
       Debrief,
       ObligationDue { obligation: ObligationId },
       OverseerRequest { request: OverseerRequestId },
   }
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum Actor { Owner, System, Overseer, OwnerDesktop }
   #[derive(Clone, Debug, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum ControlAction { Resume, Pause, Close, Archive, Restore, Clean }
   #[derive(Clone, Debug, Serialize, Deserialize)]
   #[serde(rename_all = "snake_case")]
   pub enum InterruptCause {
       DaemonStopped,
       Owner,
       Timeout,
       BackendError { message: String },
       HeadMoved { stale_for: String },
   }

Core: the doorbell bus and the clock
------------------------------------

.. code-block:: rust

   // src/core/bus.rs
   #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
   pub enum Doorbell { Thread, Outbox, Supervisor, Attention, Reporter, Config }

   pub struct Bus {
       notify: HashMap<Doorbell, Arc<tokio::sync::Notify>>,
       threads: tokio::sync::mpsc::UnboundedSender<ThreadId>,
   }
   impl Bus {
       pub fn ring(&self, bell: Doorbell) { self.notify[&bell].notify_one() }
       pub fn ring_thread(&self, id: &ThreadId) { let _ = self.threads.send(id.clone()); }
       pub async fn wait(&self, bell: Doorbell) { self.notify[&bell].notified().await }
   }

   // src/core/clock.rs — injectable, so the replay harness sets time to received_at
   pub trait Clock: Send + Sync {
       fn now(&self) -> f64;
       fn sleep_until(&self, t: f64) -> BoxFuture<'_, ()>;
   }
   pub struct SystemClock;
   pub struct ReplayClock { now: AtomicU64 }

Store
-----

.. code-block:: rust

   // src/store/mod.rs
   type Job = Box<dyn FnOnce(&mut rusqlite::Connection) + Send + 'static>;

   pub struct Store {
       tx: mpsc::Sender<Job>,
       path: PathBuf,
   }

   impl Store {
       /// single-daemon lock, WAL, foreign keys; schema::migrate(); refuses another owner's database
       pub async fn open(path: &Path, owner: &SlackUserId) -> Result<Store> { todo!() }

       pub async fn with<T, F>(&self, f: F) -> Result<T>
       where
           F: FnOnce(&mut rusqlite::Connection) -> rusqlite::Result<T> + Send + 'static,
           T: Send + 'static,
       {
           let (reply, rx) = tokio::sync::oneshot::channel();
           self.tx.send(Box::new(move |c| { let _ = reply.send(f(c)); })).await?;
           Ok(rx.await??)
       }

       /// message + thread upsert + inbox row + obligation (if it mentions the owner), one tx
       pub async fn intake(&self, msg: Message, owner: &SlackUserId) -> Result<Intake> { todo!() }
       pub async fn claim_next(&self, thread: &ThreadId) -> Result<Option<Claimed>> { todo!() }
       /// StaleSession on a version mismatch
       pub async fn commit(&self, thread: &ThreadId, item: &Claimed, out: Outcome)
           -> Result<ThreadSession> { todo!() }
       pub async fn retry_later(&self, item: &Claimed, err: &anyhow::Error) -> Result<()> { todo!() }
       pub async fn drop_with_audit(&self, item: &Claimed, err: &anyhow::Error) -> Result<()> {
           todo!()
       }
       /// running jobs → interrupted, approvals expire, sending posts → ambiguous
       pub async fn recover(&self) -> Result<Recovery> { todo!() }

       // one module per repository: messages, threads, work (jobs, workers), outbox, notes,
       // approvals, obligations, work_items (mirror), reports, watermarks
   }

.. code-block:: rust

   // src/store/schema.rs
   pub const VERSION: u32 = 5;

   pub fn migrate(c: &mut Connection) -> rusqlite::Result<()> {
       let v = current_version(c)?;
       if v > VERSION { return Err(too_new(v)) }
       if v < 4 { return Err(needs_python_first(v)) }   // 0.3.4 must have brought it to v4
       if v < 5 { v5::apply(c)? }
       Ok(())
   }

   mod v5 {
       pub fn apply(c: &mut Connection) -> rusqlite::Result<()> {
           let tx = c.transaction()?;
           tx.execute_batch(r#"
               ALTER TABLE messages ADD COLUMN mentions_owner INTEGER NOT NULL DEFAULT 0;
               CREATE INDEX IF NOT EXISTS messages_mentions
                   ON messages (channel, mentions_owner, received_at);
               ALTER TABLE threads ADD COLUMN control_json TEXT NOT NULL DEFAULT '"Active"';
               ALTER TABLE threads ADD COLUMN throttled_until REAL;
               ALTER TABLE outbox ADD COLUMN answers_json TEXT NOT NULL DEFAULT '[]';
               ALTER TABLE jobs ADD COLUMN clearance TEXT NOT NULL DEFAULT 'worker';
               ALTER TABLE jobs ADD COLUMN stale_for TEXT NOT NULL DEFAULT '';
               ALTER TABLE jobs ADD COLUMN retry_of TEXT NOT NULL DEFAULT '';
               CREATE TABLE obligations (
                   id TEXT PRIMARY KEY,
                   session_id TEXT NOT NULL REFERENCES threads (id),
                   kind TEXT NOT NULL,
                   source_json TEXT NOT NULL,
                   summary TEXT NOT NULL,
                   created REAL NOT NULL,
                   due REAL,
                   state TEXT NOT NULL DEFAULT 'open',
                   state_json TEXT NOT NULL DEFAULT '{}',
                   updated REAL NOT NULL);
               CREATE INDEX obligations_open ON obligations (state, created);
               CREATE INDEX obligations_due ON obligations (state, due);
               CREATE TABLE work_items (
                   id TEXT PRIMARY KEY,
                   kind TEXT NOT NULL,
                   repo TEXT NOT NULL,
                   number INTEGER,
                   branch_json TEXT,
                   head_sha TEXT NOT NULL DEFAULT '',
                   head_tree TEXT NOT NULL DEFAULT '',
                   base_sha TEXT NOT NULL DEFAULT '',
                   owner TEXT NOT NULL DEFAULT '',
                   state TEXT NOT NULL,
                   state_json TEXT NOT NULL DEFAULT '{}',
                   needs_json TEXT NOT NULL DEFAULT '[]',
                   due REAL,
                   queue_pos INTEGER,
                   updated REAL NOT NULL);
               CREATE TABLE reports (
                   channel TEXT NOT NULL,
                   day TEXT NOT NULL,
                   data_json TEXT NOT NULL,
                   markdown TEXT NOT NULL,
                   created REAL NOT NULL,
                   PRIMARY KEY (channel, day));
               CREATE TABLE channel_watermarks (
                   channel TEXT PRIMARY KEY,
                   last_complete_pass REAL NOT NULL,
                   pinned INTEGER NOT NULL DEFAULT 0);
           "#)?;
           backfill_mentions(&tx)?;
           derive_control_json(&tx)?;   // rule-paused threads are resumed; owner pauses kept
           move_watermarks(&tx)?;
           tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('schema_version', '5')", [])?;
           tx.commit()
       }
   }

Threads: policy, actor, outcome
-------------------------------

.. code-block:: rust

   // src/threads/policy.rs  (pure)
   pub struct Rules<'a> {
       pub owner: &'a SlackUserId,
       pub limits: &'a Limits,
       pub attention: &'a AttentionCfg,
       pub general_messages: bool,
       pub cooling: bool,
       pub observe_only: bool,
       pub resumed: bool,
   }

   #[derive(Clone, Debug)]
   pub struct Verdict {
       pub kind: VerdictKind,
       pub reason: String,
       pub turn: u32,
       pub obligation: bool,
       pub escalate: bool,
   }
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub enum VerdictKind { Ignore, Observe, Notice, Triage, Respond }

   pub fn gate(msg: &Message, s: &ThreadSession, r: &Rules) -> Verdict { /* §5 */ todo!() }

   pub struct Advance<'a> {
       pub send: bool,
       pub status: ThreadStatus,
       pub text: &'a str,
       pub turn: u32,
       pub delegated: bool,
       pub working: bool,
       pub note_kind: NoteKind,
       pub addressed_us: bool,
       pub summary: String,
       pub decisions: Vec<String>,
       pub context: StickyContext,
       pub now: f64,
   }

   pub fn advance(s: &ThreadSession, a: Advance) -> ThreadSession {
       let digest = if a.send { reply_hash(a.text) } else { String::new() };
       // R3: silence on unaddressed traffic is neutral
       let quiet = (a.send && digest == s.last_reply_hash)
           || a.note_kind == NoteKind::Ack
           || (!a.send && !a.delegated && a.addressed_us);
       let neutral = !a.send && !a.delegated && !a.addressed_us;
       let mut n = s.clone();
       if a.send {
           n.turns = n.turns.max(a.turn);
           n.wait_streak = if a.status == ThreadStatus::Waiting { n.wait_streak + 1 } else { 0 };
           n.last_reply_hash = digest;
       }
       if quiet { n.quiet_streak += 1 } else if !neutral { n.quiet_streak = 0 }
       n.status = if a.delegated || a.working { ThreadStatus::Working }
                  else if a.send { a.status } else { s.status };
       if !a.summary.is_empty() { n.summary = a.summary }
       n.decisions.extend(a.decisions);
       n.decisions = n.decisions.split_off(n.decisions.len().saturating_sub(MAX_DECISIONS));
       n.context = s.context.merge(&a.context);
       n   // no pause: the sweeper turns wait_streak / quiet_streak >= streak_signal into a signal
   }

.. code-block:: rust

   // src/threads/actor.rs
   pub struct Outcome {
       pub verdict: Option<(String, String)>,     // stored on the message row
       pub session: Option<ThreadSession>,
       pub posts: Vec<Post>,                      // outbox rows in order; each may carry `answers`
       pub jobs: Vec<NewJob>,
       pub workers: Vec<NewWorker>,
       pub controls: Vec<WorkerControl>,
       pub note: Option<Note>,
       pub calls: Vec<ParentCallRecord>,
       pub obligations: Vec<ObligationChange>,    // Create | Set(id, state) | Escalate(source, reason)
       pub defer: Option<f64>,                    // throttled: leave the item pending until then
       pub follow_ups: Vec<InboxItem>,            // e.g. Debrief after a finished discussion
       pub overseer_requests: Vec<OverseerRequest>,
       pub cooldown: Option<f64>,
   }

   pub struct ThreadActor {
       id: ThreadId,
       session: ThreadSession,
       rt: Arc<Runtime>,
   }

   impl ThreadActor {
       /// §4: claim → handle → commit; retry ≤ 3, then drop with audit
       pub async fn run(mut self) -> Result<()> { todo!() }

       async fn handle(&mut self, item: &Claimed) -> Result<Outcome> {
           use InboxItem::*;
           match &item.item {
               Message { event } => {
                   let msg = self.rt.store.message(event).await?;
                   self.on_message(&msg).await
               }
               WorkerResult { job } => self.on_worker(job).await,
               WorkerInterrupted { job, cause } => self.on_interrupted(job, cause).await,
               Control { action, actor } => self.on_control(action, *actor).await,
               OwnerInstruction { text } => self.on_owner_instruction(text).await,
               Debrief => self.on_debrief().await,
               ObligationDue { obligation } => self.on_obligation_due(obligation).await,
               OverseerRequest { request } => self.on_overseer_request(request).await,
           }
       }

       async fn on_message(&mut self, msg: &Message) -> Result<Outcome> { /* §4 */ todo!() }

       async fn decide(&mut self, msg: &Message, out: &mut Outcome) -> Result<()> {
           if let Some(not_before) = self.throttle(msg, self.rt.clock.now()) {     // §5 ceilings
               out.defer = Some(not_before);                 // the item stays pending; no model call
               out.obligations.push(ObligationChange::Escalate(msg.into(), Reason::Throttled));
               return Ok(());
           }
           let open = self.rt.store.open_obligations(&self.id).await?;
           let opts = ContextOpts { obligations: open };
           let ctx = context::build(&self.rt, &self.session, msg, opts).await?;
           let action = self.rt.parent.decide(&ctx).await?;            // one repair round inside
           let world = self.world().await?;
           let action = actions::validate(action, &world)?;
           self.apply(msg, action, out).await                          // P6: job lines from rows
       }
   }

Parent: schemas as types
------------------------

.. code-block:: rust

   // src/parent/schemas.rs
   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct Reply {
       pub send: bool,
       pub text: String,
       pub details: String,
       pub status: ReplyStatusStr,
       pub discussion: Discussion,
   }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct Delegation {
       pub worker_id: String,
       pub machine: String,
       pub tags: Vec<String>,
       pub workspace: String,
       pub backend: BackendStr,
       pub role: Role,
       pub ephemeral: bool,
       pub brief: String,
       pub deliverable: Deliverable,
   }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct Note {
       pub kind: NoteKind,
       pub repo: String,
       pub blocker: String,
       pub assignee: String,
       pub next_step: String,
   }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct ObligationDisposition {
       pub id: String,
       pub disposition: Disposition,   // answered | declined | deferred
       pub reason: String,
       pub until: String,
   }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct Ask { pub what: String, pub due: String, pub from: String }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct OverseerRequest { pub item: String, pub request: OverseerRequestKind }
   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(rename_all = "snake_case")]
   pub enum OverseerRequestKind { Rebase, Remind, Restate, Register }

   #[derive(Debug, Deserialize, JsonSchema)]
   #[serde(deny_unknown_fields)]
   pub struct Action {
       pub reply: Reply,
       pub delegations: Vec<Delegation>,
       pub controls: Vec<WorkerControl>,
       pub context: ContextPatch,
       pub summary: String,
       pub decisions: Vec<String>,
       pub note: Note,
       pub obligations: Vec<ObligationDisposition>,
       pub asks: Vec<Ask>,
       pub overseer_requests: Vec<OverseerRequest>,
   }

   /// strict: every property required, no extras (Codex's structured output needs this)
   pub fn action_schema() -> serde_json::Value { schemars::schema_for!(Action).into() }

Workers: slots, protocol, supervisor
------------------------------------

.. code-block:: rust

   // src/workers/slots.rs
   pub struct SlotPool {
       inner: Mutex<HashMap<MachineName, Vec<Option<WorkerId>>>>,
       gpus: HashMap<MachineName, Vec<u32>>,
   }
   pub struct SlotGuard {
       pub machine: MachineName,
       pub slot: u32,
       pub gpus: Vec<u32>,
       pub subfolder: Option<PathBuf>,
       pool: Weak<SlotPool>,
   }
   impl SlotPool {
       /// the sticky slot or the first free one; GPUs split as in 0.3
       pub fn acquire(self: &Arc<Self>, m: &MachineName, w: &WorkerId, sticky: Option<u32>)
           -> Option<SlotGuard> { todo!() }
       fn release(&self, m: &MachineName, slot: u32) { todo!() }
   }
   impl Drop for SlotGuard {
       fn drop(&mut self) {
           if let Some(p) = self.pool.upgrade() { p.release(&self.machine, self.slot) }
       }
   }

.. code-block:: rust

   // src/workers/protocol.rs
   #[async_trait]
   pub trait AgentProcess: Send {
       async fn start(spec: &WorkerSpec, t: &dyn Transport) -> Result<Self> where Self: Sized;
       async fn run_turn(&mut self, brief: &str, resume: Option<&BackendSessionId>,
                         schema: &Value) -> Result<()>;
       /// decoded, backend-specific events mapped to the common Event
       async fn next_event(&mut self) -> Result<Event>;
       async fn answer_approval(&mut self, id: &RequestId, decision: Decision) -> Result<()>;
       /// turn/interrupt or a control_request interrupt; queued until the turn id is known
       async fn interrupt(&mut self) -> Result<()>;
       /// reaps; the only source of the exit status
       async fn wait(&mut self) -> Result<ExitStatus>;
       async fn kill_group(&mut self) -> Result<()>;
   }

   pub enum Event {
       SessionKnown(BackendSessionId),
       Output(String),
       ApprovalRequest(ApprovalRequest),
       TurnCompleted(WorkerResult),
       Error(BackendError),
       Other,
   }

   // codex.rs: JSON-RPC over JSONL; initialize, initialized, thread/start|resume, turn/start
   // claude.rs: stream-json in and out, --permission-prompt-tool stdio, control_request/response

.. code-block:: rust

   // src/workers/supervisor.rs
   pub struct Supervisor {
       cfg: Arc<Config>,
       store: Store,
       bus: Arc<Bus>,
       pool: Arc<SlotPool>,
       running: HashMap<WorkerId, Running>,
       broker: ApprovalBroker,
   }

   impl Supervisor {
       pub async fn run(self: Arc<Self>) -> Result<()> {
           loop {
               self.schedule().await?;
               self.bus.wait(Doorbell::Supervisor).await;
           }
       }
       /// one job per worker; max_jobs per machine and globally; max_workers with idle eviction;
       /// overseer-clearance jobs first
       pub async fn schedule(&self) -> Result<Vec<JobId>> { todo!() }

       async fn run_job(&self, job: JobId, cancel: CancellationToken) -> JobEnd { /* §4 */ todo!() }

       async fn finish(&self, job: &JobId, end: JobEnd) -> Result<()> {
           use JobEnd::*;
           match end {
               // R7: once, with retry_of = job, continuing the same backend session
               RetrySameSession(e) => self.store.requeue_retry(job, &e).await,
               // result + artifacts + worker_result inbox row, one transaction
               Finished(r) => self.store.finish_job(job, JobStatus::Done, Some(r)).await,
               Failed(e) => self.store.finish_job(job, JobStatus::Failed { error: e }, None).await,
               Interrupted | TimedOut | Exited(_) => {
                   self.store.finish_job(job, JobStatus::Interrupted, None).await
               }
           }
       }

       /// F9: queued jobs whose brief names old_head → cancelled, stale_for = old_head
       pub async fn cancel_stale(&self, item: &WorkItemId, old_head: &str) -> Result<u32> { todo!() }
   }

Transports and the git wrapper
------------------------------

.. code-block:: rust

   // src/exec/transport.rs
   #[async_trait]
   pub trait Transport: Send + Sync {
       fn name(&self) -> &str;
       /// ssh: `exec sh -c …` under setsid with the FIFO watchdog when `watchdog` is set
       async fn spawn(&self, cmd: &Command, env: &Env, cwd: &Path, watchdog: bool)
           -> Result<ChildIo>;
       /// doctor
       async fn probe(&self) -> Result<Probe>;
   }
   pub struct LocalTransport;
   pub struct SshTransport { host: String, control: ControlMaster }
   pub struct SlurmTransport;   // registered, validated, spawn() returns NotImplemented

.. code-block:: rust

   // src/overseer/gitwrap.rs — installed as `git` ahead of PATH in overseer jobs
   pub fn check_push(args: &[String], allowed: &[RemotePattern], forbidden: &[RepoRef])
       -> Result<Vec<String>, Refusal>
   {
       // rewrite --force to --force-with-lease=<ref>:<expected>;
       // refuse any remote outside `allowed`; refuse any refspec into `forbidden`
       todo!()
   }

Attention
---------

.. code-block:: rust

   // src/attention/mod.rs
   pub struct Obligation { /* §5 */ }

   pub enum ObligationKind {
       Mention,
       Ask,
       Waiting { of: SlackUserId },
       JobReport { job: JobId },
       OwnerInstruction,
   }

   pub enum ObligationState {
       Open,
       Escalated { at: f64, to: EscalationTarget },
       Answered { by: OutboxId },
       Declined { reason: String, posted: Option<OutboxId> },
       Closed { by: Actor },
       Expired,
   }

   pub fn obligations_from(msg: &Message, s: &ThreadSession, v: &Verdict, owner: &SlackUserId,
                           now: f64) -> Vec<ObligationChange> {
       let mut out = vec![];
       let by_person = msg.sender != *owner && !msg.generated_by(owner);
       if v.obligation || (msg.mentions(owner) && by_person) {
           out.push(ObligationChange::Create(Obligation::mention(msg, now)));
       }
       out
   }

   pub fn obligations_from_action(a: &Action, trigger: &Message, now: f64, tz: &Tz)
       -> Vec<ObligationChange>
   {
       let asks = a.asks.iter().filter_map(|ask| {
           let due = parse_due(&ask.due, now, tz)?;
           Some(ObligationChange::Create(Obligation::ask(trigger, ask, due, now)))
       });
       let dispositions = a.obligations.iter().filter_map(|d| {
           Some(ObligationChange::Set(d.id.parse().ok()?, d.into()))
       });
       asks.chain(dispositions).collect()
   }

   pub struct Ceilings { pub max_replies_per_hour: u32, pub max_echo_replies_per_hour: u32 }
   pub struct RecentReplies {
       pub count: u32, pub echo_count: u32, pub oldest: f64, pub oldest_echo: f64,
   }
   impl Ceilings {
       /// Some(not_before) when the reply must wait; counts come from outbox rows of the last hour
       pub fn check(&self, recent: &RecentReplies, generated_trigger: bool) -> Option<f64> {
           if generated_trigger && recent.echo_count >= self.max_echo_replies_per_hour {
               return Some(recent.oldest_echo + 3600.0);
           }
           if recent.count >= self.max_replies_per_hour { return Some(recent.oldest + 3600.0) }
           None
       }
   }

   pub async fn sweep(rt: &Runtime) -> Result<()> { /* §5 */ todo!() }

   pub async fn run(rt: Arc<Runtime>) -> Result<()> {
       loop {
           sweep(&rt).await?;
           tokio::select! {
               _ = rt.clock.sleep(60.0) => {}
               _ = rt.bus.wait(Doorbell::Attention) => {}
           }
       }
   }

Overseer
--------

.. code-block:: rust

   // src/overseer/registry.rs
   pub struct WorkItem { /* §6 */ }
   pub enum WorkState { /* §6 */ }

   pub enum Need {
       SignOff { from: Person, kind: SignOffKind },   // Read | Cuda | Author
       Ci,
       Rebase,
       Rerun { check: String },
       Merge { by: Person },
       Answer { from: Person, what: String },
   }

   pub enum Evidence {
       CiRun { id: u64, conclusion: Conclusion, sha: String, at: f64 },
       RangeDiff { old: String, new: String, equal: u32, bang: u32, at: f64 },
       SignOff { from: Person, sha: String, tree: String, at: f64, location: SignOffLocation },
       Test { name: String, passed: u32, total: u32, sha: String, gpu: Option<String> },
   }

.. code-block:: rust

   // src/overseer/clearance.rs
   pub enum Action {
       RebaseAndPush { item: WorkItemId, onto: Commit },
       PostRangeDiff { item: WorkItemId, rd: RangeDiffResult },
       RequestReSign { item: WorkItemId, from: Vec<Person> },
       RerunChecks { item: WorkItemId, head: String },
       Remind { item: WorkItemId, to: Person, need: Need },
       Reassign { item: WorkItemId, need: Need, to: Person },
       RestateInFreshThread { item: WorkItemId, to: Person },
       MarkReady { item: WorkItemId },
       NotifyMerger { item: WorkItemId, to: Person },
       SignOff { item: WorkItemId, evidence: Vec<Evidence> },
       OpenDraft { item: WorkItemId },
       EditBody { item: WorkItemId, block: StatusBlock, review_section: Vec<Evidence> },
       ClosePr { item: WorkItemId, folded_into: WorkItemId },
       InterpretDecision { message: EventId },
       AskLead { item: WorkItemId, question: String },
       NotifyOwnerOfItem { item: WorkItemId, why: String },
       ResumeThread { thread: ThreadId, reason: String },
       CancelStale { item: WorkItemId, old_head: String },
       Register { item: WorkItem },
       // deliberately absent: Merge, SquashMerge, DeleteBranch, PushUpstream,
       // EditProtection, ChangeSettings
   }

   pub struct Clearance {
       fork_remotes: Vec<RemotePattern>,
       upstream: Vec<RepoRef>,
       max_posts_per_hour: u32,
       reminder_grace: f64,
   }

   impl Clearance {
       pub fn allows(&self, a: &Action, ctx: &TickContext) -> Result<(), Refusal> {
           use Action::*;
           let specific = match a {
               RebaseAndPush { item, .. } => ensure_fork_branch(ctx.item(item), &self.fork_remotes),
               Remind { item, to, .. } => {
                   ensure_grace(ctx.last_reminder(item, to), self.reminder_grace)
               }
               SignOff { item, evidence } => {
                   signoff_preconditions(ctx.item(item), evidence, ctx.github(item))   // §6
               }
               ResumeThread { thread, .. } => ensure_not_owner_paused(ctx.thread(thread)),
               _ => Ok(()),
           };
           specific?;
           ensure_post_budget(ctx.posts_last_hour(), self.max_posts_per_hour, a)
       }
   }

.. code-block:: rust

   // src/overseer/planner.rs  (pure)
   pub struct Plan { pub actions: Vec<Action> }

   pub fn next(item: &WorkItem, reg: &Registry, cfg: &OverseerCfg, now: f64) -> Plan {
       let mut a = vec![];
       if let Some(old) = item.head_moved_since_last_tick() {
           a.push(Action::CancelStale { item: item.id, old_head: old });
           // stale sign-offs are marked and `needs` recomputed by the registry on record()
       }
       if item.state.is_behind() && item.branch.is_some() {
           a.push(Action::RebaseAndPush { item: item.id, onto: item.base.clone().unwrap() });
       }
       if item.ci_failed_or_cancelled() && !reg.rerun_tried(item.id, &item.head) {
           a.push(Action::RerunChecks { item: item.id, head: item.head_sha() });
       }
       for need in item.needs.iter().filter(|n| n.is_signoff() && !item.has_evidence_for(n)) {
           if reg.person_away(need.person()) {
               a.push(match reg.substitute(need) {
                   Some(to) => Action::Reassign { item: item.id, need: need.clone(), to },
                   None => Action::AskLead {
                       item: item.id,
                       question: format!("{} is away; who takes {}?", need.person(), need),
                   },
               });
           } else if now - reg.asked_at(item.id, need) > cfg.reminder_grace {
               a.push(Action::Remind { item: item.id, to: need.person().clone(),
                                       need: need.clone() });
           }
       }
       if reg.linked_thread_stalled(item) {
           a.push(Action::RestateInFreshThread { item: item.id, to: item.owner.clone() });
       }
       if item.all_needs_met() && !item.state.is_ready() {
           a.push(Action::MarkReady { item: item.id });
           a.push(Action::NotifyMerger { item: item.id, to: reg.merger(item) });
       }
       Plan { actions: a }
   }

.. code-block:: rust

   // src/bin/fridica-overseer.rs
   #[tokio::main]
   async fn main() -> Result<()> {
       use fridica::{config, control::ControlClient, github::GithubClient, overseer::Overseer};
       let cfg = config::load()?;
       let control = ControlClient::connect(&cfg.control_socket, Actor::Overseer)?;
       let gh = GithubClient::new(env_token(&cfg.overseer.github_token_env)?)?;
       gh.assert_scopes(&cfg.overseer).await?;   // §9: refuses an over-scoped token
       let mut ov = Overseer::open(cfg, control, gh).await?;
       loop {
           if let Err(e) = ov.tick().await { tracing::error!(?e, "tick failed") }
           ov.sleep_or_signal().await;
       }
   }

Reporter and MCP
----------------

.. code-block:: rust

   // src/report/mod.rs
   pub struct ReportData {
       pub channel: ChannelId,
       pub day: NaiveDate,
       pub needs_you: NeedsYou,
       pub yesterday: Yesterday,
       pub open_work: OpenWork,
       pub overseer: OverseerActions,
       pub health: Health,
   }
   pub async fn collect(store: &Store, channel: &ChannelId, day: LocalDay) -> Result<ReportData> {
       todo!()
   }
   pub fn markdown(d: &ReportData, prose: Option<&str>) -> String { todo!() }
   /// every number, sha and id in the prose must appear in the data
   pub fn check_prose(prose: &str, d: &ReportData) -> Result<(), Vec<String>> { todo!() }
   pub async fn run(rt: Arc<Runtime>) -> Result<()> { /* §7 */ todo!() }

.. code-block:: rust

   // src/mcp/mod.rs (run by `fridica mcp`)
   #[derive(Clone)]
   struct FridicaMcp { control: ControlClient, allow_control: bool }

   #[rmcp::tool(tool_box)]
   impl FridicaMcp {
       #[tool(description = "Configured channels and whether today's report exists")]
       async fn list_channels(&self) -> Json<Vec<ChannelView>> {
           self.control.get("/channels").await
       }

       #[tool(description = "The daily report for a channel: sections as JSON plus Markdown")]
       async fn daily_report(&self, #[tool(param)] channel: String,
                             #[tool(param)] date: Option<String>) -> Json<ReportView> { todo!() }

       #[tool(description = "What needs the owner: escalated and overdue obligations, \
                             approvals, paused and blocked threads, items waiting for a human")]
       async fn attention(&self) -> Json<AttentionView> { self.control.get("/attention").await }

       #[tool(description = "Obligations, optionally filtered by thread and state")]
       async fn obligations(&self, #[tool(param)] thread: Option<String>,
                            #[tool(param)] state: Option<String>) -> Json<Vec<ObligationView>> {
           todo!()
       }

       #[tool(description = "Work items from the overseer's registry")]
       async fn work_items(&self, #[tool(param)] repo: Option<String>,
                           #[tool(param)] state: Option<String>) -> Json<Vec<WorkItemView>> {
           todo!()
       }

       #[tool(description = "One thread in full")]
       async fn thread(&self, #[tool(param)] id: String) -> Json<ThreadView> { todo!() }

       // registered only when allow_control: close_obligation, resume_thread, pause_thread,
       // instruct_thread, decide_approval, register_work_item
   }

   pub fn run() -> Result<()> {   // called by the `mcp` subcommand of the fridica binary
       if std::env::var_os("FRIDICA_WORKER").is_some() { std::process::exit(0) }   // §7
       let key = std::env::var("FRIDICA_MCP_KEY").ok();
       tokio::runtime::Runtime::new()?.block_on(async {
           // without a valid key the client connects but the server advertises no tools
           let control = ControlClient::connect_with_key(default_socket()?, key.as_deref(),
                                                         Actor::OwnerDesktop).await?;
           let server = FridicaMcp { control, allow_control: config::mcp().allow_control };
           rmcp::serve_stdio(server).await
       })
   }

Control API routes
------------------

.. code-block:: text

   GET  /status
   GET  /threads                       GET  /threads/{id}
   POST /threads/{id}/{resume|pause|close|archive|restore|clean|instruct}
   GET  /workers                       POST /workers/{id}/{interrupt|stop}
   GET  /approvals                     POST /approvals/{id}        (once | session | deny)
   GET  /outbox                        POST /outbox/{id}/retry
   GET  /machines
   GET  /settings                      POST /settings              (owner only)
   GET  /attention                     GET  /attention/threads     (0.3 compatible)
   GET  /obligations                   POST /obligations/{id}/close
   GET  /reports                       GET  /reports/{channel}/{date}
   POST /reports/{channel}/{date}/regenerate
   GET  /work-items                    PUT  /work-items/{id}       (overseer only: the mirror)
   GET  /overseer/requests             POST /overseer/requests/{id}/ack
   POST /posts                         (overseer only: an outbox row of kind overseer)
   POST /jobs                          (overseer only: a job with clearance overseer)

   Every request passes the socket's uid check (mode 0600) and carries an Actor header;
   every mutating route writes an audit row with that actor.

Configuration additions
-----------------------

.. code-block:: toml

   [attention]
   mention_grace = 900
   max_replies_per_hour = 20
   max_echo_replies_per_hour = 6
   streak_signal = 3
   ask_status_turns = true          # drive a status turn when an ask comes due

   [overseer]
   enabled = false
   interval = 600
   summary_interval = 3600
   reminder_grace = 7200
   max_posts_per_hour = 12
   github_token_env = "FRIDICA_OVERSEER_GITHUB_TOKEN"
   fork_remotes = []                # e.g. ["UCzhangxi/*", "chengcli/*"]
   upstream_repos = []              # e.g. ["chengcli/snapy", "chengcli/kintera"]
   timezones = ["America/Los_Angeles", "America/New_York", "Asia/Shanghai"]
   merge_windows = ["09:00", "17:00"]   # in the first timezone; readiness reminders align to them
   state = "~/.local/state/fridica/overseer.sqlite3"

   [report]
   time = "07:00"
   timezone = "America/New_York"
   prose = true
   post_to_channel = false
   keep_days = 90
   dir = "~/.local/state/fridica/reports"

   [mcp]
   allow_control = false
   key = "~/.local/state/fridica/mcp.key"

Python launcher package
-----------------------

.. code-block:: python

   # python/fridica/__init__.py
   from .__main__ import main

   __all__ = ["main"]

   # python/fridica/__main__.py
   import os
   import sys
   from pathlib import Path


   def main() -> None:
       here = Path(__file__).resolve().parent
       binary = here / "bin" / "fridica"
       if not binary.exists():
           sys.exit(f"fridica: bundled binary missing at {binary}; reinstall the wheel")
       os.execv(str(binary), [str(binary), *sys.argv[1:]])


   if __name__ == "__main__":
       main()

Parity exceptions file
----------------------

.. code-block:: toml

   # tests/parity_exceptions.toml — every allowed difference between spec/ (Python 0.3.4)
   # and the Rust core on the corpus; anything else fails the run
   [[exception]]
   section = "5"
   reason = "no automatic pause: the mentions 0.3 observed in paused threads are answered"
   python = { verdict = "observe: thread is paused", mentions_owner = true }
   rust = { verdict = "respond: addressed" }
   expected_count = 89

   [[exception]]
   section = "5"
   reason = "the 18 rule pauses are not created; streaks of 3 raise a signal instead"
   effect = "control stays active; signal obligations created"
   expected_count = 18
