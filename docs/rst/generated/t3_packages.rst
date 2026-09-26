.. list-table:: Packages of src/fridica
   :header-rows: 1
   :widths: 14 8 78

   * - Package
     - Lines
     - Responsibility
   * - app.py
     - 246
     - Composition root: wires store, Slack, parent, actors, supervisor, control API; recovery; hot reload
   * - core
     - 380
     - Value types (sessions, workers, jobs, results, posts), errors, doorbell bus, injectable clock
   * - store
     - 1,118
     - SQLite: the only DDL (versioned migrations), one repository per table, crash recovery
   * - config
     - 636
     - Typed schema with the single set of defaults, loader with every cross-field rule, editor, discovery
   * - machines
     - 338
     - Machine registry, policies, resources, GPU/slot views, selector resolution
   * - slack
     - 526
     - Socket Mode ingress, Web API egress, durable outbox dispatcher, catch-up, links, rendering, metadata
   * - threads
     - 664
     - One serial actor per thread, pure gate/advance policy, bounded parent context
   * - parent
     - 763
     - Tool-less structured-output LLM calls, prompts, schemas, action validation and repair, contract, repos
   * - workers
     - 1,218
     - JSONL worker processes (Claude, Codex), WorkerResult parsing, artifacts, supervisor with slots
   * - exec
     - 522
     - Transports (local, ssh, slurm stub), process plumbing, SSH watchdog, bubblewrap confinement
   * - approvals
     - 133
     - Approval broker (persist, wait, timeout) and policy rules
   * - control
     - 374
     - Daemon JSON API on a 0600 Unix socket and its client
   * - dashboard
     - 90
     - Localhost web UI proxying the control API behind a per-run key
   * - doctor
     - 202
     - Environment, backend, sandbox, and GPU checks on every machine
   * - cli
     - 198
     - Command line: init, configure, doctor, start, dashboard, control commands
