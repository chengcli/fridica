The baseline: 0.3.3 and what is in flight
=========================================

The 0.3 design in one figure
----------------------------

.. code-block:: text

                        Slack (Socket Mode, the owner's user token)
                                        │
                              persist, then ack
                                        │
                              thread_inbox (SQLite)
                                        │
             ┌──────────────────────────┼──────────────────────────┐
        ThreadActor A             ThreadActor B               ThreadActor C   (one serial actor per thread)
             │                         │                           │
        policy.gate ──▶ triage / decide (tool-less parent CLI, JSON schema)
             │
        apply: outbox posts, jobs, workers, session, notes, parent_turns — one transaction
             │
        Supervisor.schedule ──▶ Worker (claude -p | codex app-server) on a machine slot
                                   │ local / ssh / (slurm)   bwrap or sandbox-exec
                                   ▼
                              WorkerResult ──▶ worker_result inbox row ──▶ the actor composes one reply
                                        │
                              OutboxDispatcher ──▶ Slack (idempotent, per-thread FIFO)

Everything above stays. The 0.3 design document (``docs/fridica-design.pdf``) describes it in detail
and is not repeated here; this document only names the pieces it changes.

The unmerged improvements
-------------------------

Six review branches exist on ``chengcli/fridica``, stacked on ``main`` (``5249e2e``) in the order
agreed in the campaign thread, each with its own tests. They came out of Xi's "requests after the
No-Human-Zone analysis" thread, where Cheng, Tianhao, chen sihe and Xi's side settled the scope. They
are not merged; each still needs two independent reviews and a ``#bot-lab`` run. 0.4 treats their
*semantics* as part of the specification (rule P1), so they are listed here with what the Rust core
inherits from each.

.. list-table:: Review branches F1–F6 and their meaning for 0.4
   :header-rows: 1
   :widths: 6 20 44 30

   * - #
     - Branch
     - What it does (Python)
     - What 0.4 inherits
   * - F1
     - ``review/fridica-f1/github-links``
     - Follows GitHub PR and issue links into the parent's context as an untrusted ``github_state``
       block: title, state, head sha and tree, base and a behind-base flag, ``mergeable`` (unknown
       after one retry), check runs with ``cancelled`` as its own state, reviews as ``approved
       @<sha>`` with stale ones marked, assignees, labels, and only the owner / next / blocker /
       waiting-on lines of the body. Cached per (repo, number); optional read-only token removed from
       every child process.
     - The ``GithubState`` type and client become the ``github`` module, shared by the parent context builder and the
       overseer, which needs exactly this state to decide the next action for a work item.
   * - F2
     - ``review/fridica-f2/catchup``
     - Catch-up from each channel's last complete pass (a watermark in ``meta``), up to 7 days back,
       including replies of threads active in the window; a truncated pass repeats from the same point;
       caught-up messages older than a day are stored but not queued unless they mention the owner.
     - The watermark and the "mention always queues" rule. The attention guarantee extends the latter:
       a mention found by catch-up creates an obligation like any other.
   * - F5
     - ``review/fridica-f5/replies``
     - A pure repeat (same text and status, nothing new) is not resent, except for an @-mention, an
       explicit repost request, a correction, the owner's instruction, or a worker result; a dropped
       repeat counts as a turn without progress and is recorded as ``suppressed_repeat``.
     - The suppression rule. The no-progress accounting changes (`The attention guarantee <attention_>`_), so a suppressed
       repeat still counts, but a deliberate non-reply to an unaddressed message does not.
   * - F4
     - ``review/fridica-f4/blocked``
     - A mention in a blocked thread gets ``Blocked: <blocker>. Next: <name> to <next_step>.`` from the
       task note, once per blocked period and blocker, plain names, no paging; the contract asks the
       parent to fill blocker, assignee and next step.
     - The notice text and its idempotency. The attention guarantee changes what happens *after* the
       notice: the mention stays an open obligation, and a mention carrying a new ask reopens the thread.
   * - F3
     - ``review/fridica-f3/attachments``
     - Reads text attachments for the parent (optional, behind ``files:read``): text mimetypes only,
       64 KB per file with a marker, at most three files and 64 KB per reply sharing the history's
       character budget, the daemon's token sent only to ``files.slack.com``, own uploads skipped,
       ``clean`` also erases attachment metadata, HTML never taken for a file when scopes are unknown.
       Adds schema v4.
     - The attachment model and its limits. Schema v4 is the last Python schema and the one the Rust
       migration starts from.
   * - F6
     - ``review/fridica-f6/upgrade-note``
     - ``docs/upgrade-v0.2.md`` with the steps, a key map and changed defaults; no ``fridica migrate``.
     - The precedent. 0.4 *does* ship ``fridica migrate`` (`Phases <migration_>`_), because a Rust binary
       replacing a Python program is a bigger step than a config rename.

Two more branches are in flight and are also part of the baseline:

- ``cli/fix-exit-status`` (its own PR): a worker whose stdout reached EOF before the process was
  reaped reported "status None". The fix waits up to two seconds for the exit status. In Rust this class
  of bug is handled structurally: the supervisor ``select!``'s over the protocol reader, the child's
  ``wait()``, cancellation and the timeout, and the exit status is read from the reaped ``Child`` only.
- ``feat/scoped-repo-fetch`` (PR #32, 2026-09-26): a scoped repository fetch for workers. It is the piece
  the overseer needs to fetch and rebase branches without giving a worker the whole network.

The F7–F9 proposals
-------------------

After F1–F6, Tianhao proposed three follow-ups and Cheng agreed to take them as new features on their
own branches, starting with a design note. 0.4 is that design note. They become first-class parts of
the Rust core rather than Python patches.

.. list-table:: F7–F9 and where this document handles them
   :header-rows: 1
   :widths: 6 40 54

   * - #
     - Proposal
     - In 0.4
   * - F7
     - Track work items separately from Slack threads: repo, PR, exact head and tree, owner, status,
       evidence; a thread can link to several items.
     - The ``work_items`` table and the ``WorkItem`` state machine, owned by the overseer and read by
       the parent (`The overseer <overseer_>`_, `Data model and schema v5 <data-model_>`_).
   * - F8
     - Let code determine permissions and queue state; let the model decide intent. A standing grant
       must not produce an approval prompt, and "queued" must only be said after the job row exists.
     - Rule P6. The reply text that announces a delegation is rendered by code from the committed job
       rows, not by the model; the approval broker consults the effective policy before it asks
       (`The Rust core <rust-core_>`_).
   * - F9
     - Separate worker reports from sign-offs: verify head, tree, required tests and CI right before
       posting a review; a changed head makes evidence stale and cancels queued work for the old sha;
       carry a sign-off forward only after an all-``=`` range-diff and green CI.
     - The overseer's ``SignOff`` action and its preconditions; the ``stale_for`` cancellation of
       queued jobs when a work item's head moves (`The overseer <overseer_>`_).

Repository and process decisions from the same thread
-----------------------------------------------------

Part 1 of Xi's list is about the repositories, not Fridica, but three of its decisions shape the
overseer: one shared record per task on the PR or issue with a fixed status block (owner, next
action, blocker, waiting-on, plus head sha and tree hash), edited only by the task owner's agent;
sign-offs as GitHub reviews bound to a sha, carried forward across an all-``=`` rebase with green CI
(cancelled is not green; a ``!`` is new code); and patches as pushed ``review/*`` branches on the
contributor's fork, never as base64. The overseer's actions are written to produce exactly these
artifacts.
