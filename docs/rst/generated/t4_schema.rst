.. list-table:: Schema v3: tables and live row counts
   :header-rows: 1
   :widths: 16 9 75

   * - Table
     - Rows
     - Holds
   * - messages
     - 1,191
     - every Slack message, including Fridica's own posts (source=self)
   * - threads
     - 77
     - thread sessions: control, status, turns, summary, decisions, sticky context
   * - thread_inbox
     - 982
     - per-thread work queue: messages, worker results, controls, debriefs
   * - parent_turns
     - 520
     - one row per parent LLM call, written with the effects it produced
   * - workers
     - 89
     - Claude/Codex sessions bound to machine, workspace, slot
   * - jobs
     - 135
     - one delegated task each; results as WorkerResult JSON
   * - artifacts
     - 39
     - files returned by jobs, validated and stored for upload
   * - outbox
     - 388
     - every Slack post, idempotent by key, ordered per thread
   * - approvals
     - 0
     - worker permission requests and decisions
   * - notes
     - 279
     - revisioned task notes per thread
   * - audit
     - 2
     - owner and policy actions
   * - cooldowns
     - 1
     - per-channel pacing of unsolicited replies
   * - runtime
     - 1
     - daemon heartbeat, Slack status, config fingerprint
   * - meta
     - 3
     - schema version and the bound Slack identity
