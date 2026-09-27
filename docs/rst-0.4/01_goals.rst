Goals and non-goals
===================

What 0.4 is
-----------

0.4 is the second overhaul of Fridica. The first (PR #22, 0.3) fixed the architecture: one serial
actor per thread, a durable inbox and outbox, a tool-less parent that delegates to workers on a machine
registry, and one SQLite database owned by one daemon. 0.4 keeps that architecture and changes four
things.

1. **The implementation language of the daemon.** The core moves from Python to Rust. The reason is
   not speed: almost none of Fridica's wall-clock time is spent in Python, and the user will not
   notice a faster daemon. The reason is that Fridica has become a concurrent, security-sensitive
   supervisor of many long-lived processes and stateful sessions across machines, and that is the kind
   of program where Rust's ownership model, exhaustive enums, typed protocol decoding and single-binary
   deployment pay for themselves. The evaluation that led to this decision is summarized in
   `Why Rust, in one page <why-rust_>`_.

2. **An attention guarantee.** A mention of the owner must end in one of three outcomes: a reply, a
   posted reason for not replying, or an escalation the owner can see, and no rule pauses a thread on
   its own. 0.3 has no such guarantee, and the last run shows the cost (`Evidence from the last run <evidence_>`_): a fifth of the mentions by other people got nothing.

3. **A standing overseer.** A separate process that keeps a campaign moving: it tracks every work item
   (a pull request, a review, a CUDA run, a sign-off), notices what is blocked, does the mechanical
   unblocking itself (rebase, force-push with lease to a review branch, re-sign requests, reminders,
   fresh self-contained restatements), and hands the human the one thing that stays human: the final
   squash-merge. Its duties are those of the PR-lead session that Xi drove by hand during the
   campaign, written down and given to a program with a bounded clearance (`The overseer <overseer_>`_).

4. **Daily reports and a desktop integration.** For every channel Fridica is in, a report per day:
   what happened, what is open, what needs the owner. Reports are Markdown files and are served by an
   MCP server that Claude Desktop and Codex Desktop can register, so the owner can ask their desktop
   agent "what does my Fridica need from me today" and have it act (`Daily reports and the desktop integration <reporting_>`_).

What 0.4 is not
---------------

- **Not a line-by-line port.** The Python tree is the executable specification. The Rust core must
  reproduce its observable behaviour on a replay corpus, but it may (and should) restructure the code
  to exploit types where Python used conventions.
- **Not a rewrite of the dashboard.** The dashboard stays vanilla JavaScript served by the daemon. No
  WASM, no new frontend framework.
- **Not an embedded-Python or PyO3 design.** The Python package becomes a thin launcher that
  ``execv``'s the Rust binary. No Python runs inside the daemon, and the daemon never imports Python.
- **Not a change of the Slack model.** One app per person, a user token, Socket Mode, one daemon per
  owner per database. Channels remain a namespace; the thread remains the unit of state.
- **Not a change of the trust model.** Model inputs are untrusted, tokens never reach agents, workers
  never spawn workers, and only the owner (in person) merges into upstream ``main``.

Guiding rules for the port
--------------------------

The following rules are the contract between the Python specification and the Rust implementation.
They are referenced throughout the document.

.. list-table:: Port rules
   :header-rows: 1
   :widths: 6 94

   * - Rule
     - Statement
   * - P1
     - **Freeze first.** F1–F6 and the exit-status fix merge into the Python tree before the port
       starts, and the resulting tag (``v0.3.4``) is the specification. New behaviour (attention,
       overseer, reporting) is designed here and implemented in Rust only.
   * - P2
     - **Port architecture, not code.** Every Python convention that encodes a state (``status``
       strings, optional fields that must be set together, ``kind`` strings on inbox rows) becomes a
       Rust enum whose invalid combinations cannot be constructed.
   * - P3
     - **The database is the interface.** Schema v4 (after F3) is the last Python schema. The Rust
       core opens a v4 database, migrates it to v5, and from then on the schema is the contract that
       the CLI, dashboard, overseer, reporter and MCP server all read through the control API.
   * - P4
     - **Same processes at the edges.** ``claude``, ``codex app-server``, ``ssh``, ``bwrap``,
       ``sandbox-exec``, ``socat`` and later ``sbatch`` remain external processes with unchanged
       command lines and protocols. Only the process that supervises them changes.
   * - P5
     - **Replay parity.** The 2.5-day corpus from the last run (messages, inbox, jobs, posts) is
       replayed through both implementations with fake Slack and fake backends; verdicts, outbox rows
       and session transitions must match, except where a design change in this document says they
       must differ, and each such difference has a named test.
   * - P6
     - **Code owns state; the model owns intent.** A queued job is "queued" only after its row is
       committed. A permission is decided by the effective policy, not by the model's belief. This
       rule is Tianhao's F8 proposal, and 0.4 makes it a design rule rather than a fix.

.. _why-rust:

Why Rust, in one page
---------------------

The evaluation asked whether Rust would be a better implementation language for the repository as it
stands. Its conclusion, which this design adopts, was that Rust wins on the properties that matter for
a supervisor daemon and loses on the ones that matter for an application that experiments with model
prompts. The daemon is the former.

.. list-table:: Where each language is stronger for Fridica's core
   :header-rows: 1
   :widths: 30 35 35

   * - Concern
     - Python 0.3
     - Rust 0.4
   * - Thread actors, serial per thread, parallel between
     - asyncio tasks and queues; discipline by convention
     - one Tokio task per actor owning its state; a channel per actor; ownership enforced at compile time
   * - Worker lifecycle (starting, running, awaiting approval, idle, interrupted, finished, failed)
     - ``status`` strings plus optional ``process``, ``session_id``, ``approval`` fields
     - one ``enum WorkerState`` whose variants carry exactly the data that state has
   * - Delivery (pending, sending, sent, rate-limited, ambiguous, failed, blocked)
     - ``state`` strings and ``retry_at``/``sent_ts`` columns interpreted by code
     - ``enum DeliveryState`` with typed payloads; the outbox table stores its serialization
   * - Backend protocols (Codex app-server JSON-RPC, Claude stream-json)
     - ``msg.get("type")`` chains; protocol drift fails late and quietly
     - Serde-tagged enums; drift fails at decode, with the offending message in the error
   * - Machine slots and GPU shares
     - integers reserved and released by code paths that must all agree
     - a ``SlotGuard`` owned by the worker; dropping the worker releases the slot
   * - Identifiers (thread, channel, worker, job, session, outbox, machine, workspace)
     - all ``str``; a job id can be passed where a worker id is expected
     - newtypes; the mix-up does not compile
   * - Process supervision (signals, timeouts, orphans, EOF before reap)
     - the exit-status race fixed on ``fix-exit-status`` is one instance of a class
     - ``tokio::process`` with ``kill_on_drop``, ``select!`` over output, cancellation and timeout
   * - Deployment
     - Python 3.11 environment plus a wheel
     - one static binary per platform inside the wheel; ``pip install fridica`` still works
   * - Prompt and schema experimentation
     - fast
     - slower; kept fast by keeping prompts, contract and schemas as data files, not code
   * - Performance
     - irrelevant
     - irrelevant

Two conditions from the evaluation are built into the plan: stabilize the semantics in Python first
(rule P1), and make the whole daemon Rust rather than a Rust extension under Python orchestration,
because there is no performance kernel to extract and the benefits are architectural. The pieces that
stay outside Rust are the ones that should: the browser dashboard, the agent CLIs, the sandboxes, and
the shells on remote machines.
