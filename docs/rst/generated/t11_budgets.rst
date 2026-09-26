.. list-table:: Context budgets and caps
   :header-rows: 1
   :widths: 45 55

   * - Item
     - Budget
   * - Thread history sent to the parent
     - last 60 messages, ≤ context_chars/2 = 12,000 characters
   * - Channel context (new threads only)
     - 10 messages, ≤ 4,000 characters
   * - Triage call history
     - last 15 messages
   * - Rolling thread summary
     - ≤ 2,000 characters
   * - Decisions kept per thread
     - last 20
   * - Delegation brief
     - ≤ 40,000 characters
   * - WorkerResult.summary / .report
     - ≤ 1,500 / ≤ 4,000 characters
   * - Artifacts per result
     - ≤ 3
   * - Slack reply (rest goes to a details file)
     - ≤ 7,000 characters
