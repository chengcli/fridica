.. _plan:

Migration, testing and rollout
==============================

.. note::

   This chapter is the historical proposal, not the current deployment checklist.
   The `active launch plan <../v0.4-active-launch-plan.md>`_ and
   `implementation status <../v0.4-implementation-status.md>`_ supersede its rollout
   order and setup assumptions. In particular, development does not require an
   installed Slack daemon, fresh installations do not migrate an old database,
   and campaign operations use existing owner gh/SSH credentials rather than new
   tokens, deploy keys or fabricated reviewer identities.

.. _migration:

Phases
------

.. list-table:: Phases of 0.4
   :header-rows: 1
   :widths: 8 24 68

   * - Phase
     - Deliverable
     - Exit criterion
   * - A
     - **Freeze.** F1–F6 reviewed (two independent reviews each, ``#bot-lab`` runs) and merged;
       ``fix-exit-status`` merged; ``feat/scoped-repo-fetch`` reviewed; tag ``v0.3.4``.
     - ``v0.3.4`` runs the next campaign; its database becomes the second replay corpus.
   * - B
     - **Parity core.** the modules ``core``, ``config``, ``store``, ``slack``, ``github``, ``machines``,
       ``threads``, ``parent``, ``workers``, ``exec``, ``approvals``, ``control``, ``dashboard``, ``doctor``,
       ``cli``; the Python launcher; ``fridica migrate``.
     - the replay harness passes on both corpora with zero unexplained differences; ``doctor`` passes
       on every machine in the owner's config; the wheel installs on Linux and macOS.
   * - C
     - **Attention.** the ``attention`` module, the revised gate, the reply ceilings and streak
       signals in place of the automatic pause, obligations in the dashboard and CLI, contract
       additions.
     - the named parity exceptions of §5 pass; a week of live use shows no mention with
       ``observe`` as its final verdict and no thread throttled without an owner-visible signal.
   * - D
     - **Overseer.** the ``overseer`` module and the ``fridica-overseer`` binary, the campaign registry, clearance, planner rules, jobs with
       overseer clearance, the parent's overseer block and requests.
     - a campaign of at least three PRs run end to end in ``#bot-lab`` with the human doing only the
       squash-merges; every action in the audit table; no push outside ``fork_remotes``.
   * - E
     - **Reports and MCP.** the ``report`` and ``mcp`` modules, ``fridica mcp``, desktop registration, the
       worker-isolation measures.
     - a day's report matches a hand count; Claude Desktop and Codex Desktop both list the tools; a
       Codex worker on the owner's machine sees no tools.
   * - F
     - **Release 0.4.0.** CI builds wheels for Linux x86_64/aarch64 and macOS; ``release.py verify``;
       PyPI.
     - ``pip install fridica==0.4.0`` on a clean machine gives a working ``fridica doctor``.

Phases C, D and E are independent of each other and can proceed in parallel once B is done; C is
the one that changes live behaviour most and should ship first.

The replay harness
------------------

The parity harness is the instrument for rule P5 and the reason phase B can be trusted.

.. code-block:: text

   corpus (state.sqlite3, read-only snapshot)
       │  messages ordered by received_at, source socket | catchup
       │  parent_turns (inbox_id → action_json)   → a fake parent returning the recorded action
       │  jobs (job_id → result_json | error)      → fake claude/codex returning the recorded result
       ▼
   driver ── feeds messages to Daemon.receive() with the Clock set to received_at
       │
       ├── Python 0.3.4 (spec/)   ─▶ verdicts, inbox kinds, outbox rows (kind, text hash, order),
       │                             session transitions
       └── Rust 0.4 (this tree)   ─▶ the same
                                          │
                                   diff ── every difference must match a line in
                                          tests/parity_exceptions.toml (section, reason, count),
                                          else the run fails

Fake executables are the 0.3 ones (``tests/fakes.py``) ported to small Rust binaries so that the
harness has no Python dependency once phase A is over. The harness runs in CI on the anonymized
corpus shipped in ``tests/corpus/`` (the docs build already anonymizes; the same anonymizer produces
the corpus).

Tests beyond parity
-------------------

- **Unit tests** per module: the gate table (every row), ``advance()``, the delivery state machine,
  slot allocation and GPU splitting, serde round-trips for every protocol message recorded from real
  backends (``tests/protocol/*.jsonl``), config validation rules, the planner rules (each with a
  registry fixture), the sign-off preconditions, the push-refspec wrapper, and the layering test.
- **Process tests**: the supervisor against the fake backends for exit-before-EOF, EOF-before-exit,
  timeout, interrupt during approval, daemon stop with a remote worker (the watchdog kills the
  process group), and R7's retry-once.
- **Integration**: the daemon against a fake Slack (Socket Mode and Web API) for intake, catch-up
  watermarks, outbox ordering, ambiguous outcomes; the overseer against a recorded GitHub fixture
  set (a PR that goes behind, CI cancelled, a stale approval, a lease failure).
- **Security**: the injection fixtures from F1 and F3; the MCP server started with
  ``FRIDICA_WORKER=1`` and without the key; the overseer refusing to start with an over-scoped token;
  a push to an upstream refspec refused by the wrapper.
- **Documentation**: the design document's numbers regenerated from the corpus, as ``docs/scripts``
  does today.

What a 0.4 user has to set up
-----------------------------

Everything a 0.3 owner already has stays valid: the Slack app and its scopes, the Claude and Codex
sign-ins on every machine, bubblewrap, socat and the AppArmor profile on Linux workers, the SSH
aliases, ``config.toml`` and ``contract.md``. The table lists only what changes.

.. list-table:: Toolchain and permissions: 0.3 → 0.4
   :header-rows: 1
   :widths: 20 26 54

   * - Area
     - 0.3
     - 0.4
   * - Installing from PyPI
     - Python ≥ 3.11 and pip
     - Python ≥ 3.9 and pip; no Rust toolchain. Wheels for Linux x86_64 and aarch64
       (manylinux_2_28, glibc ≥ 2.28) and macOS x86_64 and arm64. A host too old for the wheel can
       still be a worker machine, since nothing Rust runs there.
   * - Installing from source
     - ``pip install -e .``
     - rustup (stable ≥ 1.80) and maturin in addition; ``maturin develop`` or ``pip install -e .``
       builds the binary. This is the one real toolchain addition, and it affects developers and
       reviewers, not ``pip`` users.
   * - Worker machines
     - Claude/Codex CLIs, bwrap + socat, AppArmor profile, ``ssh`` alias, git
     - unchanged; git ≥ 2.19 (``range-diff``) on machines the overseer uses for rebase jobs.
       ``doctor`` checks it.
   * - Slack
     - user token scopes, ``connections:write`` app token; ``files:read`` with F3
     - **no change.** Reports, notices and overseer posts use the existing ``chat:write`` and
       ``files:write``. The manifest is the 0.3.4 one.
   * - GitHub, F1 (optional)
     - none
     - a read-only fine-grained token in ``[github] token_env``, or anonymous access (60 calls per
       hour per IP) for public repositories.
   * - GitHub, overseer
     - none
     - a fine-grained token in ``FRIDICA_OVERSEER_GITHUB_TOKEN``: on the owner's repositories and
       forks *Contents* read/write, *Pull requests* read/write, *Actions* read/write; on upstream
       repositories the owner does not want it to push to, *Contents* read, *Pull requests* and
       *Issues* read/write, *Actions* read, *Metadata* read. Never *Contents* write on upstream and
       never a classic ``repo`` scope; the overseer refuses to start otherwise.
   * - GitHub, repository owner (once per repository)
     - none
     - branch protection on ``main``: pull request required, pushes restricted to the owner;
       collaborator access for the reviewing agents' accounts so sign-offs can be GitHub reviews;
       for another person's fork the overseer should push to, a write deploy key added by that
       fork's owner (§6).
   * - Machine running rebase jobs
     - push credentials for the owner's repositories (already needed by workers)
     - the same, plus git identity and, if upstream requires signed commits, the signing key, since
       a rebase rewrites commits; ``github.com`` in the workspace's ``[policy] network`` (or the
       scoped repository fetch).
   * - Processes and services
     - one daemon (``fridica start``)
     - two when the overseer is enabled; example systemd user units and a launchd plist ship with
       the package (``fridica service print``).
   * - State directory
     - ``state.sqlite3``, ``control.sock``, dashboard key
     - plus ``overseer.sqlite3``, ``reports/``, ``mcp.key`` (0600); ``fridica init`` creates them,
       ``fridica migrate`` backs up before touching the database.
   * - Claude Desktop
     - none
     - paste ``fridica mcp --print-config`` into ``claude_desktop_config.json`` (an absolute path to
       the binary: GUI applications do not see the shell's ``PATH``) and restart the app.
   * - Codex Desktop / CLI
     - none
     - ``[mcp_servers.fridica]`` under a *profile* in ``~/.codex/config.toml``, not at top level, so
       the ``codex app-server`` workers the daemon starts do not load it; ``doctor`` warns when it
       is top-level.
   * - ``config.toml`` keys
     - ``[limits] max_wait_replies``, ``max_no_progress``
     - both removed (unknown keys are errors, so ``fridica migrate`` rewrites them: their value
       becomes ``[attention] streak_signal``); new ``[attention]``, ``[overseer]``, ``[report]``
       and ``[mcp]`` tables with defaults, so an unchanged 0.3 file needs only the rewrite.
   * - Environment variables
     - ``SLACK_APP_TOKEN``, ``SLACK_USER_TOKEN``
     - plus ``FRIDICA_OVERSEER_GITHUB_TOKEN`` (overseer process), ``FRIDICA_MCP_KEY`` (desktop app
       only), the optional F1 token; all three are stripped from every worker.
   * - Outbound network from the daemon host
     - Slack (Socket Mode and Web API)
     - plus ``api.github.com`` (F1, overseer) and ``files.slack.com`` (F3).

Nothing in the table changes a permission that workers have; the sandbox and policy model of 0.3 is
untouched.

Rollout on the owner's machine
------------------------------

1. ``pip install fridica==0.4.0`` into the same environment; ``fridica doctor`` (the Rust one) on
   every machine.
2. ``fridica migrate --dry-run`` prints the plan; ``fridica migrate`` backs up and migrates.
3. ``fridica start --observe-only`` for one catch-up window; then ``fridica start``.
4. ``[overseer] enabled = true`` only after a ``#bot-lab`` campaign; ``[mcp] allow_control = true``
   only after a week of read-only desktop use.
5. Keep the 0.3.4 environment for two weeks; ``fridica migrate --rollback`` restores the backup and
   is refused once the v5 database has more than the migration's own audit rows.

Open questions
--------------

- **Slurm.** Unchanged from 0.3: registered, validated, not runnable. The Rust transport trait leaves
  room for ``ssh login 'srun … codex app-server'``; allocation lifetime versus worker idle time is
  still open.
- **Workers across restarts.** Still not survived. ``codex app-server daemon`` with ``proxy`` over SSH
  is the candidate; the typed ``WorkerState`` makes a ``Reattached`` variant cheap to add later.
- **Who runs the overseer when several owners share a campaign.** The design assumes one overseer per
  campaign, run by the lead's owner; two overseers on one campaign would both remind and both
  rebase. A ``campaigns.lead`` field and a refusal to act on a campaign led by another owner is the
  minimum; a shared registry is out of scope.
- **Merge queue.** If snapy and kintera move to an organization, GitHub's merge queue removes the
  rebase-and-re-sign loop that the overseer automates. The overseer's rules would shrink to
  readiness and reminders; nothing in this design prevents that.
- **``asks`` extraction quality.** Whether the parent reliably extracts due times from free text is an
  empirical question; phase C measures the ratio of ``Ask`` obligations to messages with a due-time
  pattern and reports it.
