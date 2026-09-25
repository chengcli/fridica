# Architecture

Fridica maps Slack onto an agent hierarchy:

- **workspace:** the parent agent (the owner);
- **thread:** a thread session;
- **delegated job:** a worker bound to one machine and one workspace;
- **machine:** an entry in the registry.

None of these relationships is one-to-one. One thread can fan out to several
machines, one machine hosts isolated workers for many threads, and one parent
serves every thread at once.

## Packages (`src/fridica/`)

| Package | Responsibility |
| --- | --- |
| `app.py` | `Daemon`, the composition root; `serve()` connects it to Slack Socket Mode |
| `core/` | value types (`models.py`), errors, the doorbell `Bus`, the injectable `Clock` |
| `store/` | SQLite: `schema.py` (the only DDL, versioned migrations), repositories, `Store.recover()` |
| `config/` | typed schema with the single set of defaults, the loader with every cross-field rule, the comment-preserving editor, the template, Slack discovery |
| `machines/` | the registry (`Machine`, `Workspace`, `Policy`, `Resources`) and selector resolution |
| `slack/` | ingress (`normalize`), egress (Web API and error mapping), the outbox dispatcher, catch-up, link following, rendering and metadata |
| `threads/` | `ThreadManager` (one actor task per busy thread), `ThreadActor`, the pure `policy`, the parent `context` builder |
| `parent/` | the tool-less structured-output LLM call, schemas, prompts, action validation with one repair round, the contract, the repository list |
| `workers/` | the JSONL process base, `CodexWorker` (app-server JSON-RPC), `ClaudeWorker` (stream-json with the control protocol), `WorkerResult` parsing, artifacts, `Supervisor` |
| `exec/` | the `Transport` protocol: `local`, `ssh` (ControlMaster, `exec sh -c`), `slurm` (stub); process plumbing; bubblewrap confinement |
| `approvals/` | the broker (persist a request, await the owner's decision, deny on timeout) and policy rules |
| `control/` | the daemon's JSON API on a 0600 Unix socket, and its client |
| `dashboard/` | a localhost server that proxies to the control socket with a per-run key; vanilla JS |
| `doctor/`, `cli/` | checks that run where each process will run; the command line |

## Flow of one message

1. **Intake.** Socket Mode, or catch-up, calls `normalize()` and then
   `Daemon.receive()`. `Messages.intake()` inserts the message, deduplicated on
   `(workspace, channel, ts)`, and upserts its thread. It also inserts a
   `thread_inbox` row, all in one transaction. The event is acked after the commit,
   and the thread's doorbell rings.
2. **Actor.** `ThreadManager` starts the thread's `ThreadActor`. The actor claims
   inbox rows in order, and `policy.gate()` decides what to do with the message:
   - `ignore` or `observe`;
   - `notice` for a blocked thread;
   - `triage`, a cheap call that decides whether to join;
   - `respond`.
3. **Parent.** `threads/context.py` builds a bounded `ParentContext` from:
   - the history;
   - the channel context for new threads;
   - linked messages;
   - the session summary, decisions and sticky context;
   - this thread's workers with their last results, minus the prose report;
   - the machine registry, with names, tags, workspace names and load, but no paths;
   - the repository list.

   `ParentAgent.decide()` returns an `Action`. `parent/actions.validate()` resolves
   selectors through `machines.match.resolve()` and enforces the per-channel and
   per-thread limits. Problems get one repair call.
4. **Apply.** One transaction writes everything the action caused:
   - outbox posts (the reply, then the details upload);
   - new worker rows and job rows (a turn's jobs share a `join_group`);
   - the advanced session (`policy.advance()`: turns, wait streak, stall counter,
     summary, decisions, sticky context, pauses);
   - task notes;
   - the cooldown;
   - `parent_turns` records;
   - the inbox row set to `done`.
5. **Outbox.** `OutboxDispatcher` posts due items in per-thread FIFO order and
   records each successful post as a `self` message.
6. **Workers.** `Supervisor.schedule()` starts queued jobs within these limits:
   - one job per worker;
   - `max_jobs` per machine and globally;
   - `max_workers` live processes per machine, evicting idle workers that have no
     queued work.

   A worker runs on its machine through its transport, and its approval requests go
   through the broker. Structured-result parsing falls back to one summarize turn,
   then to prose. A finished job commits its result, its artifacts, and a
   `worker_result` inbox row.
7. **Results.** The actor waits until every job in the group is done, then composes
   one reply. A single finished job with a report is posted directly. Jobs are
   marked `reported`, so a group is composed exactly once.

Owner controls (resume, pause, close, archive, restore, clean) are `control` inbox
rows. They are therefore serialized with everything else in the thread.

## Backend protocols (verified against Claude Code 2.1.282 and codex-cli 0.154)

- **Codex worker:** `codex app-server` speaking JSON-RPC over JSONL.
  - Setup: `initialize`, `initialized`, then `thread/resume` or `thread/start`
    (with `approvalPolicy`, `sandbox`, and `developerInstructions`).
  - Each job: `turn/start` with a `sandboxPolicy` and the WorkerResult
    `outputSchema`, ending at `turn/completed`.
  - Server requests `item/commandExecution/requestApproval` and
    `item/fileChange/requestApproval` are answered with `accept`, `acceptForSession`
    or `decline`. `item/permissions/requestApproval` is answered with
    `{permissions, scope}`.
  - Interrupting sends `turn/interrupt`. An interrupt that arrives before the turn
    id is known is queued and sent once it is.
- **Claude worker:** `claude -p --input-format stream-json --output-format stream-json
  --permission-prompt-tool stdio`, plus `--append-system-prompt` and a `--settings`
  sandbox document.
  - Tool calls outside the allowlist arrive as `control_request` with subtype
    `can_use_tool`, and are answered with a `control_response` whose `behavior` is
    `allow` or `deny`.
  - The documented `--permission-prompts host` alone does not route prompts to the
    host; the stdio prompt tool is required.
  - Interrupting sends a `control_request` with subtype `interrupt`.
- **Parent:** a one-shot `claude -p --json-schema … --tools ""
  --no-session-persistence`, or `codex exec --output-schema … --ephemeral` with
  tools off. Tool use is detected and rejected.

## Deliberate limits and open items

- **Slurm.** Registered and validated, but spawning raises. The intended shape is
  `ssh login 'srun … codex app-server'`. Still open: allocation lifetime compared
  with worker idle time, and queue waits.
- **Workers across daemon restarts.** Workers do not survive a daemon restart; jobs
  that were running become interrupted. A future option is `codex app-server daemon`
  with `proxy` over SSH, the same pattern Codex Desktop uses.
- **Approvals.** Claude's `can_use_tool` control protocol is internal to the Agent
  SDK. `policy.claude_prompts = "none"` falls back to deny-all prompts.
- **Codex configuration.** `codex app-server` loads the machine owner's
  `~/.codex/config.toml`; doctor warns when it defines MCP servers.
