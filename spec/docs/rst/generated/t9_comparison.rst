.. list-table:: Legacy vs overhaul
   :header-rows: 1
   :widths: 20 38 42

   * - Aspect
     - Legacy (PR #1–#21)
     - Overhaul (PR #22–)
   * - Thread processing
     - one global asyncio lock for every thread
     - one serial actor per thread; threads parallel
   * - Work per thread
     - one backend session, ≤ 1 heavy worker
     - many workers with roles (max 14 in one thread), fan-out and joins
   * - Machines
     - derived from host:/path roots; primary host runs every turn
     - machine registry with tags, policies, slots, GPU split
   * - Reply agent
     - CLI run with tools, per turn
     - tool-less parent; tools only in workers
   * - Durability of posts
     - replies only; notices, wrap-ups, debriefs best effort
     - every post through the outbox (388 posts, 0 unsent)
   * - Schema
     - DDL in 6 modules; tasks row with 20 columns
     - one versioned schema (v3), 14 tables
   * - Worker output
     - free prose, ATTACH: lines
     - structured WorkerResult (status, changes, validation, artifacts)
   * - Loop control
     - turn limit with wrap-ups (5 continuations)
     - wait-streak and no-progress pauses; no turn limit
   * - Permissions
     - Fridica-executed file operations with approvals
     - backend sandboxes + native approvals (auto)
   * - Worker jobs per day
     - 4.7
     - 100
   * - Mean Slack reply
     - 938 characters
     - 773 characters
