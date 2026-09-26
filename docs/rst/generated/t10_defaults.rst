.. list-table:: Configuration defaults (read from the code)
   :header-rows: 1
   :widths: 55 45

   * - Setting
     - Default
   * - limits.max_wait_replies
     - 3
   * - limits.max_no_progress
     - 3
   * - limits.max_delegations_per_turn
     - 3
   * - limits.max_workers_per_thread
     - 4
   * - limits.max_jobs
     - 4
   * - limits.parent_concurrency
     - 4
   * - limits.job_timeout
     - 4 h
   * - limits.worker_idle
     - 30 min
   * - limits.session_timeout
     - 14 days
   * - limits.auto_resume
     - false
   * - limits.reply_chars
     - 7000
   * - limits.report_fast_path
     - true
   * - machines.<name>.max_workers
     - 4
   * - machines.<name>.max_jobs
     - 2
   * - policy.mode
     - write
   * - policy.network
     - (none)
   * - policy.approvals
     - auto
   * - policy.approval_timeout
     - 30 min
   * - policy.auto_approve
     - (none)
   * - policy.auto_deny
     - (none)
   * - policy.gpu_confine
     - automatic (write-mode workspaces on GPU machines)
   * - policy.claude_prompts
     - host
   * - parent.backend
     - claude
   * - parent.model
     - (backend CLI default)
   * - parent.triage_model
     - (backend CLI default)
   * - parent.reasoning_effort
     - (backend CLI default)
   * - parent.timeout
     - 3 min
   * - parent.context_chars
     - 24000
   * - parent.default_machine
     - the first configured machine
