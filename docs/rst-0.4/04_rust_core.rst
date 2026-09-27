.. _rust-core:

The Rust core
=============

Processes and binaries
----------------------

0.4 ships one Cargo package, ``fridica``, that builds two binaries and one Python wheel.

.. list-table:: Processes in 0.4
   :header-rows: 1
   :widths: 22 14 64

   * - Process
     - Binary
     - Role
   * - the daemon
     - ``fridica``
     - ``fridica start``: Slack, actors, parent calls, supervisor, outbox, control API, dashboard,
       reporter. Owns the state database. One per owner.
   * - the overseer
     - ``fridica-overseer``
     - a standing process with its own database and its own clearance; talks to the daemon over the
       control socket and to GitHub and git directly (`The overseer <overseer_>`_). Optional; one per owner.
   * - the MCP server
     - ``fridica`` (``fridica mcp``)
     - a stdio MCP server started by Claude Desktop or Codex Desktop; a client of the control socket,
       never of the database (`Daily reports and the desktop integration <reporting_>`_).
   * - the CLI and doctor
     - ``fridica``
     - ``init``, ``configure``, ``doctor``, ``status``, ``threads``, ``workers``, ``approvals``,
       ``outbox``, ``report``, ``obligations``, ``migrate``, ``dashboard``; all read through the
       control socket except ``init``, ``doctor`` and ``migrate``.
   * - the Python launcher
     - ``python -m fridica``
     - ``execv`` of the bundled ``fridica`` binary. Exists so that ``pip install fridica`` keeps
       working.

The daemon, the overseer and the MCP server are separate OS processes on purpose. The daemon must
stay simple and must be the only writer of the state database; the overseer holds a broader
clearance and must be killable without stopping Slack; the MCP server is started and stopped by a
desktop application and must never hold the database.

Source layout
-------------

The Rust source is **one crate with modules**, not a workspace of crates. The modules mirror the
0.3 Python packages one to one, so ``spec/fridica/store/`` and ``src/store/`` describe the same
thing during the port and a reviewer can read them side by side.

.. code-block:: text

   fridica/
   ├── Cargo.toml            one package: lib `fridica` + bins `fridica`, `fridica-overseer`
   ├── src/
   │   ├── lib.rs            declares the modules below; nothing else
   │   ├── bin/
   │   │   ├── fridica.rs            the daemon, CLI, doctor, migrate, dashboard, `fridica mcp`
   │   │   └── fridica-overseer.rs   the overseer (§6)
   │   ├── core/             ids, models, state machines, errors, clock, bus, policy (the gate)
   │   ├── config/           serde schema, loader with cross-field rules, toml_edit editor, template
   │   ├── store/            rusqlite (bundled), migrations v4→v5…, repositories, corpus reader
   │   ├── slack/            Socket Mode, Web API, ingress, egress, outbox dispatcher, catch-up, files
   │   ├── github/           the F1 client and GithubState; used by parent and overseer
   │   ├── machines/         the registry and selector resolution
   │   ├── threads/          ThreadManager, ThreadActor, context builder
   │   ├── parent/           the tool-less call: contract, prompts, schemas, actions + validation, repos
   │   ├── workers/          protocol, Claude and Codex adapters (serde), supervisor, slots, results
   │   ├── exec/             transports: local, ssh (ControlMaster), slurm (stub); sandbox; watchdog
   │   ├── approvals/        the broker and the policy rules
   │   ├── attention/        obligations, pause policy, sweeper, escalation (§5)
   │   ├── overseer/         work items, clearance, planner, campaign loop, git wrapper (§6)
   │   ├── report/           daily reports: collector, renderer, storage (§7)
   │   ├── control/          the JSON API over the Unix socket, its client, views
   │   ├── mcp/              the MCP server (rmcp) over the control client (§7)
   │   ├── dashboard/        the localhost server; static assets embedded with include_dir
   │   ├── doctor/           checks that run where each process will run
   │   └── cli/              clap commands
   ├── dashboard/            unchanged vanilla JS (embedded)
   ├── assets/               contract.md, repos.toml, template.toml, manifest.yaml (embedded)
   ├── python/fridica/       the launcher package (__init__.py, __main__.py)
   ├── pyproject.toml        maturin, bindings = "bin"
   ├── spec/                 the frozen Python 0.3.4 tree, used only by the parity harness
   └── tests/                integration tests, the replay harness, the layering test, fakes

Why modules and not crates. Splitting into crates would buy three things: Cargo-enforced acyclic
layering, per-binary dependency isolation, and finer incremental compilation. At the size this port
will reach (roughly 15–25 thousand lines) none of them is worth fifteen manifests, ``pub`` versus
``pub(crate)`` decisions at every boundary, and cross-crate refactors while the types are still
moving, which is the one place the Rust evaluation warned that Rust slows a design down. The two
binaries both link every dependency (``fridica-overseer`` carries the Slack client it never uses);
that costs binary size, not correctness, since the overseer never receives the Slack tokens. If a
module boundary starts to hurt later, moving a module into its own crate is a mechanical change.

Two rules replace what Cargo would have enforced:

- **Layering is a test.** ``tests/layers.rs`` reads every ``use crate::…`` in ``src/`` and checks it
  against the four layers of the 0.3 design (interfaces → coordination → execution → foundation,
  downward only; ``core`` and ``config`` are foundation, ``store`` may be used by all). A ``use``
  that points upward fails the test. The appendix has the skeleton.
- **The module is called core.** Inside the crate ``mod core`` is legal and mirrors 0.3, but a
  bare path ``core::fmt`` would then be ambiguous with Rust's ``core`` crate. Code writes ``std::…``
  for the standard library (derive macros already emit ``::core::…`` with a leading ``::``, so they
  are unaffected), and Clippy's ``std_instead_of_core`` lint stays off.

Dependencies, with the reason for each:

.. list-table:: Dependencies
   :header-rows: 1
   :widths: 26 74

   * - Dependency
     - Use
   * - ``tokio`` (rt-multi-thread, process, net, sync, time, signal)
     - the runtime; one task per actor, per worker, per transport connection
   * - ``serde``, ``serde_json``
     - every protocol message, every stored JSON column, every control API body
   * - ``rusqlite`` (bundled, ``serde_json`` feature)
     - the state database; SQLite compiled in so the binary has no system dependency
   * - ``slack-morphism`` (hyper, socket-mode)
     - Socket Mode and Web API; the client wrapper keeps the same error mapping as ``slack/egress.py``
   * - ``reqwest`` (rustls)
     - file downloads (F3) and the GitHub REST calls (F1); rustls avoids OpenSSL on remote builds
   * - ``octocrab``
     - typed GitHub API for the overseer (pulls, reviews, check runs, comments); F1 reads use it too
   * - ``toml``, ``toml_edit``
     - config parsing with unknown-key errors; comment-preserving edits for ``configure`` and the
       dashboard settings
   * - ``clap`` (derive)
     - the CLI
   * - ``tracing``, ``tracing-subscriber``
     - structured logs; the audit table is written from a tracing layer
   * - ``thiserror``, ``anyhow``
     - typed errors inside modules, context at the binary edges
   * - ``nix``, ``libc``
     - process groups, ``killpg`` (EPERM tolerated on macOS), FIFOs for the SSH watchdog, socket modes
   * - ``hyper`` + ``hyperlocal``, ``axum``
     - the control API on a Unix socket, and the dashboard's localhost HTTP server
   * - ``rmcp``
     - the MCP server (stdio transport, tools and resources)
   * - ``include_dir``
     - embeds the dashboard and the packaged assets
   * - ``uuid``, ``sha2``, ``hex``, ``chrono``, ``chrono-tz``
     - ids, idempotency keys, report times in the owner's timezone

Git is driven through the ``git`` executable, never ``libgit2``: the overseer must use the owner's
``~/.ssh/config``, credential helpers and signing setup unchanged, and it must run on remote machines
through the same transport as workers.

Runtime shape
-------------

.. code-block:: text

   main ─ Daemon::serve
     ├─ slack::socket_mode ─▶ ingress::normalize ─▶ store.intake (message + thread + inbox,
     │                                                one tx) ─▶ bus.ring(thread)
     ├─ slack::catchup      (watermarks, F2)                              │
     ├─ ThreadManager       ── one ThreadActor task per thread with pending items ◀┘
     │       └─ actor.handle(item) ── gate ── parent (CLI, JSON schema) ── apply (one tx)
     │                                 ── bus.ring(outbox | supervisor)
     ├─ OutboxDispatcher    ── per-thread FIFO, DeliveryState machine, idempotency keys
     ├─ Supervisor          ── schedule jobs ── Worker tasks (Claude | Codex adapter over a
     │                         Transport) ── results as inbox rows
     ├─ Attention           ── obligation sweeper, pause expiry, escalation (§5)
     ├─ Reporter            ── daily collector + renderer per channel (§7)
     ├─ ControlApi          ── Unix socket 0600; the only path for CLI, dashboard, overseer, MCP
     └─ ConfigWatcher       ── reloads config.toml; a rejected edit keeps the running config

Every arrow that crosses a task boundary is a ``tokio::sync::mpsc`` channel or the doorbell ``Bus``.
Every arrow that crosses a process boundary is either JSONL on a pipe (agents), JSON over the Unix
socket (control), or a committed SQLite row (everything durable).

Identifiers and models
----------------------

All identifiers are newtypes. This is the cheapest and most effective change of the port: a system
that carries Slack ``ts`` strings, channel ids, thread ids, session ids, worker ids, job ids, outbox
ids and machine names cannot afford to have them all be ``String``.

.. code-block:: rust

   // src/core/ids.rs
   macro_rules! string_id {
       ($name:ident) => {
           #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
           #[serde(transparent)]
           pub struct $name(pub String);
       };
   }
   string_id!(WorkspaceId);      // Slack team
   string_id!(ChannelId);
   string_id!(SlackTs);          // "1790333192.674889"; ordered as a decimal string
   string_id!(EventId);
   string_id!(MachineName);
   string_id!(WorkspaceName);
   string_id!(BackendSessionId);
   string_id!(WorkerId);         // 0.3 ids ("wece9b672") are kept; new ones are UUID strings
   string_id!(JobId);            // same ("j939cd1cec1")

   pub struct ThreadId { workspace: WorkspaceId, channel: ChannelId, root_ts: SlackTs }
   // Display and parse() keep the 0.3 form "{workspace}:{channel}:{root_ts}"

   pub struct ObligationId(Uuid);
   pub struct WorkItemId(Uuid);
   pub struct OutboxId(i64);
   pub struct InboxId(i64);

Existing rows keep their 0.3 string ids; the migration does not rewrite them. The appendix has the
exact definitions with their derives.

The thread session is the same record as in 0.3, with the ``control`` and ``status`` strings replaced
by enums that carry their reasons:

.. code-block:: rust

   pub enum ThreadControl {
       Active,
       Paused { by: Actor, reason: String, since: f64 },   // owner or overseer; never a rule
       Closed,
       Archived,
       Cleaned,
   }
   pub enum ThreadStatus { New, Complete, Waiting, Blocked, Working }

The inbox is an enum, so an actor cannot receive a kind it does not handle:

.. code-block:: rust

   pub enum InboxItem {
       Message { event: EventId },
       WorkerResult { job: JobId },
       WorkerInterrupted { job: JobId, cause: InterruptCause },
       Control { action: ControlAction, actor: Actor },
       OwnerInstruction { text: String },
       Debrief,
       ObligationDue { obligation: ObligationId },      // new: §5
       OverseerRequest { request: OverseerRequestId },  // new: §6
   }

Thread actors
-------------

The actor is the same object as ``threads/actor.py``: it claims the oldest pending inbox item of its
thread, handles it, and commits the outcome in one transaction. In Rust it owns its ``ThreadSession``
between items and communicates only through channels.

.. code-block:: rust

   pub struct ThreadActor {
       id: ThreadId,
       session: ThreadSession,
       rt: Arc<Runtime>,
   }

   impl ThreadActor {
       pub async fn run(mut self) -> Result<()> {
           loop {
               // exits when the thread's inbox is empty; the manager restarts it on the next ring
               let Some(item) = self.rt.store.claim_next(&self.id).await? else { return Ok(()) };
               let outcome = match self.handle(&item).await {
                   Ok(outcome) => outcome,
                   Err(e) if item.attempts < 3 => {
                       self.rt.store.retry_later(&item, &e).await?;
                       continue;
                   }
                   Err(e) => {
                       self.rt.store.drop_with_audit(&item, &e).await?;
                       continue;
                   }
               };
               // one transaction; StaleSession on a version mismatch with an owner action
               self.session = self.rt.store.commit(&self.id, &item, outcome).await?;
               self.rt.bus.ring(Doorbell::Outbox);
               self.rt.bus.ring(Doorbell::Supervisor);
           }
       }
   }

The handler for a message is the 0.3 sequence with the attention rules of `The attention guarantee
<attention_>`_ inserted at the gate:

.. code-block:: rust

   async fn on_message(&mut self, msg: &Message) -> Result<Outcome> {
       let verdict = policy::gate(msg, &self.session, &self.rt.rules());   // pure; §5 has the table
       let mut out = Outcome::for_verdict(&verdict, msg);                  // records the verdict
       out.obligations.extend(attention::obligations_from(msg, &self.session, &verdict));   // R4
       match verdict.kind {
           Ignore | Observe => {}
           Notice => out.posts.push(render::blocked_notice(&self.session)),   // F4, once per period
           Triage => {
               if self.parent.triage(self.context(msg)).await? == Join {
                   self.decide(msg, &mut out).await?;
               }
           }
           Respond => self.decide(msg, &mut out).await?,   // may defer instead: the §5 ceilings
       }
       Ok(out)
   }

``decide`` first checks the reply ceilings of §5 (a throttled item is left pending with a
``not_before`` time and no model is called), then calls the parent CLI, validates the action against the thread's world, runs one
repair round, and turns the surviving action into rows. Rule P6 is applied here in two places: the
reply text that announces "queued on <machine>" is composed by ``render::delegation_line`` from the
job rows the transaction will insert, and the parent's own ``queued``/``started`` wording is replaced
by a placeholder it must use (``{{jobs}}``) or the line is appended.

Parent calls
------------

The parent stays a tool-less one-shot CLI call with a JSON schema. The schemas move from Python dicts
to Rust types that derive both ``Deserialize`` and a JSON Schema (``schemars``), so the schema the CLI
is given and the type the answer is decoded into cannot diverge. The ``Action`` type gains fields for
`The attention guarantee <attention_>`_ and `The overseer <overseer_>`_:

.. code-block:: rust

   #[derive(Deserialize, JsonSchema)]
   pub struct Action {
       pub reply: Reply,                    // send, text, details, status, discussion
       pub delegations: Vec<Delegation>,
       pub controls: Vec<WorkerControl>,
       pub context: StickyContextPatch,
       pub summary: String,
       pub decisions: Vec<String>,
       pub note: Note,                      // kind, repo, blocker, assignee, next_step
       pub obligations: Vec<ObligationDisposition>,   // {id, answered|declined{reason}|deferred{until}}
       pub asks: Vec<Ask>,                  // explicit asks in the trigger: {what, due, from}
       pub overseer_requests: Vec<OverseerRequest>,   // {item, rebase|remind|restate|register}
   }

Worker supervision
------------------

The supervisor keeps the 0.3 scheduling rules (one job per worker, ``max_jobs`` per machine and
globally, ``max_workers`` live processes with idle eviction) and changes how a running worker is
represented.

.. code-block:: rust

   pub enum WorkerState {
       Configured,
       Starting { slot: SlotGuard },
       Running {
           slot: SlotGuard, proc: AgentProcess, session: BackendSessionId,
           job: JobId, turn: Option<TurnId>,
       },
       AwaitingApproval {
           slot: SlotGuard, proc: AgentProcess, session: BackendSessionId,
           job: JobId, request: ApprovalRequest,
       },
       Idle { slot: SlotGuard, proc: AgentProcess, session: BackendSessionId, since: f64 },
       Interrupted { session: Option<BackendSessionId>, cause: InterruptCause },
       Finished { result: WorkerResult, session: BackendSessionId },
       Failed { error: WorkerError, session: Option<BackendSessionId> },
       Stopped,
   }

A ``SlotGuard`` is the machine slot the worker holds, with its GPU share and subfolder; dropping it
releases the slot in the ``SlotPool``. A worker cannot be ``Running`` without a slot and cannot hold
a slot after it is ``Stopped``, because the variant does not have the field.

.. code-block:: rust

   pub struct SlotGuard {
       machine: MachineName,
       slot: u32,
       gpus: Vec<u32>,
       subfolder: Option<PathBuf>,
       pool: Weak<SlotPool>,
   }
   impl Drop for SlotGuard {
       fn drop(&mut self) {
           if let Some(pool) = self.pool.upgrade() { pool.release(&self.machine, self.slot) }
       }
   }

Running a job is one ``select!``, which is where the exit-status race and the orphan problems of 0.3
are handled structurally:

.. code-block:: rust

   async fn run_job(&mut self, brief: Brief, cancel: CancellationToken) -> JobEnd {
       let deadline = tokio::time::sleep(self.limits.job_timeout);
       tokio::pin!(deadline);
       loop {
           tokio::select! {
               ev = self.proc.next_event() => match ev? {     // serde-typed CodexEvent | ClaudeEvent
                   Event::ApprovalRequest(r) => self.route_approval(r).await?,   // policy first (P6)
                   Event::TurnCompleted(res) => return JobEnd::Finished(res),
                   Event::Error(e) if e.retryable() && self.attempt == 0 => {
                       return JobEnd::RetrySameSession(e);                       // R7
                   }
                   Event::Error(e) => return JobEnd::Failed(e.into()),
                   _ => {}
               },
               status = self.proc.wait() => {
                   return JobEnd::Exited(status?);            // the real exit status, after reaping
               }
               _ = cancel.cancelled() => {
                   self.proc.interrupt().await?;
                   return JobEnd::Interrupted;
               }
               _ = &mut deadline => {
                   self.proc.kill_group().await?;
                   return JobEnd::TimedOut;
               }
           }
       }
   }

Backend protocols
-----------------

Both backends' streams are decoded into tagged enums. Unknown message types are kept as ``Other``
and logged at debug level, so protocol additions do not break the daemon, while a change to a
message the daemon depends on fails at decode with the offending line in the error.

.. code-block:: rust

   #[derive(Deserialize)]
   #[serde(tag = "method", content = "params")]
   pub enum CodexServerRequest {
       #[serde(rename = "item/commandExecution/requestApproval")]
       CommandApproval { id: RequestId, command: Vec<String>, cwd: PathBuf },
       #[serde(rename = "item/fileChange/requestApproval")]
       FileChangeApproval { id: RequestId, paths: Vec<PathBuf> },
       #[serde(rename = "item/permissions/requestApproval")]
       PermissionsApproval { id: RequestId, permissions: Vec<String> },
       #[serde(other)]
       Other,
   }

   #[derive(Deserialize)]
   #[serde(tag = "type", rename_all = "snake_case")]
   pub enum ClaudeEvent {
       System { subtype: String, session_id: Option<BackendSessionId> },
       Assistant { message: Value },
       ControlRequest { request_id: String, request: ClaudeControlRequest },  // can_use_tool
       Result {
           subtype: ResultSubtype, session_id: BackendSessionId,
           structured_output: Option<Value>, is_error: bool,
       },
       #[serde(other)]
       Other,
   }

The ``doctor`` protocol checks (approvals, ``turn/interrupt``, ``outputSchema``) are the same
decoders run against the backend's ``initialize`` answer.

Store
-----

SQLite is opened by one dedicated thread that owns the ``Connection``; every repository method is an
``async fn`` that sends a closure to that thread and awaits its result. This keeps the 0.3 guarantee
that all effects of an inbox item commit in one transaction, keeps WAL mode and the single-daemon
lock, and avoids sharing a connection across Tokio workers. The parity harness (`Migration, testing
and rollout <plan_>`_) uses the same store against a copy of the corpus database.

.. code-block:: rust

   pub struct Store {
       tx: mpsc::Sender<Box<dyn FnOnce(&mut rusqlite::Connection) + Send>>,
   }
   impl Store {
       pub async fn with<T, F>(&self, f: F) -> Result<T>
       where
           F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
           T: Send + 'static,
       { /* send f to the connection thread, await a oneshot */ }

       pub async fn commit(&self, thread: &ThreadId, item: &Claimed, out: Outcome)
           -> Result<ThreadSession>
       {
           self.with(move |c| {
               let tx = c.transaction()?;
               // session (versioned), posts, jobs, workers, notes, parent_turns,
               // obligations, verdict on the message, the item marked done
               tx.commit()?;
               Ok(session)
           }).await
       }
   }

Schema v5 and the migration from v4 are in `Data model and schema v5 <data-model_>`_.

Configuration
-------------

``config.toml`` keeps its tables and its "unknown keys are errors" rule; the loader is a ``serde``
struct with ``deny_unknown_fields`` and the cross-field rules of ``config/loader.py`` as a
``validate()`` pass. New tables are ``[attention]``, ``[overseer]``, ``[report]`` and ``[mcp]``. The
comment-preserving editor uses ``toml_edit``. The full additions are in the appendix; the defaults
that matter for behaviour are:

.. code-block:: toml

   [attention]
   mention_grace = 900              # seconds a mention may stay unanswered before it escalates
   max_replies_per_hour = 20        # per thread, to non-owner triggers; over it: defer + escalate
   max_echo_replies_per_hour = 6    # per thread, replies to triggers carrying Fridica metadata
   streak_signal = 3                # wait/quiet streak that raises a signal to the owner (no pause)

   [overseer]
   enabled = false
   interval = 600               # seconds between campaign ticks
   summary_interval = 3600      # campaign summary cadence; 0 disables
   github_token_env = "FRIDICA_OVERSEER_GITHUB_TOKEN"
   fork_remotes = ["UCzhangxi/*", "chengcli/*"]              # force-with-lease pushes allowed here
   upstream_repos = ["chengcli/snapy", "chengcli/kintera"]   # PR edits only, never pushes

   [report]
   time = "07:00"               # local time of the owner's timezone
   timezone = "America/New_York"
   post_to_channel = false      # also post the report into the channel as the owner
   keep_days = 90

   [mcp]
   allow_control = false        # whether MCP tools may resume/pause/instruct threads

Control API, dashboard, CLI
---------------------------

The control API keeps its URL space and adds ``/obligations``, ``/reports``, ``/work-items`` and
``/overseer``. The dashboard is unchanged apart from an *Attention* view that now also lists open and
overdue obligations, and an *Overseer* view. The CLI maps one to one onto the API. The API is the
only surface the overseer and the MCP server use, which is what keeps them out of the database.

The Python launcher
-------------------

.. code-block:: python

   # python/fridica/__main__.py
   import os, sys
   from pathlib import Path

   def main() -> None:
       binary = Path(__file__).parent / "bin" / "fridica"
       os.execv(binary, [str(binary), *sys.argv[1:]])

.. code-block:: toml

   # pyproject.toml
   [build-system]
   requires = ["maturin>=1.7,<2"]
   build-backend = "maturin"

   [project]
   name = "fridica"
   dynamic = ["version"]
   requires-python = ">=3.9"    # the launcher has no dependencies; 3.11 is no longer required

   [tool.maturin]
   bindings = "bin"
   python-source = "python"
   # both binaries from the root Cargo.toml go into the wheel; the launcher execs `fridica`

Wheels are built on CI for ``x86_64`` and ``aarch64`` Linux (manylinux) and macOS. The version comes
from the git tag through ``cargo`` and ``maturin``; ``scripts/release.py verify`` keeps checking that
the wheel bundles the manifest, contract, repository list, template and dashboard, now by listing the
binary's embedded assets (``fridica assets --list``).
