.. _overseer:

The overseer
============

What the PR-lead session did
----------------------------

During the campaign Xi ran, beside an observe-only Fridica, a separate Claude Code session that posted
as Xi through the user token. The database records what that session did for 2.5 days, and it is the
specification for the overseer. Its recurring actions, with the message forms it used:

.. list-table:: Duties of the PR-lead session, observed
   :header-rows: 1
   :widths: 26 74

   * - Duty
     - Observed form
   * - Keep one table of state
     - every summary lists each item with head sha (and tree), base, *0 behind* or *behind*, CI run id and
       verdict, who signed at which sha, what it needs, and its owner: *"snapy #223 draft, head 97b3be4
       (tree 08d679f), 0 behind, CI green. Signed: chen sihe, Cheng's agent. Needs: Tianhao."*
   * - Post periodic summaries
     - hourly, then per campaign, *"only what changed since"*; three time zones in the header; credit by name
   * - Assign asks with due times
     - one ask per person per summary, with a due time and the exact command or line wanted
   * - Remind and check status
     - *"A status check (17:52 PT) on your three items. Two are past due. Please post what you have now,
       even if partial, and a new time for the rest."*; *"gentle reminder"*; *"Reminder, re-sent as a fresh
       thread in case the earlier one got lost"*
   * - Restate in a fresh, self-contained thread
     - when a bot's thread was stuck, paused or compacted: *"Fresh thread for Cheng's agent … the PR thread
       may not be reaching it. The three asks, restated in full."*
   * - Rebase after main moves, push, request re-signs
     - *"The worker rebased xiz/spr10-output from 914b43b onto …"*; posts ``git range-diff`` counts (*8/8 '='*)
       and asks each reviewer whose approval is stale to re-sign at the new head
   * - Open, undraft and edit PRs
     - opens drafts on the fork branch, keeps one PR *ready* at a time in merge order, updates the body's
       Review section with every sign-off, closes PRs folded into others
   * - Declare readiness, hand the merge to the human
     - *"#213 is READY to squash-merge (Cheng, for you in person; it's the last in the queue). Lead-checked
       just now: head … 0 behind. CI run …: all green. Re-sign: … range-diff 8/8 '='."*
   * - Reassign when someone is away
     - *"chen sihe is asleep in China, so Cheng's agent takes the CUDA checks … the queue no longer waits
       for you"*
   * - Apply the human's decisions to the queue
     - *"Queue change from Xi: fewer PRs … #122 absorbs #123 … 6 PRs left instead of 9"*; *"New rule from
       Xi: no snapy PR counts as ready without a CUDA run"*
   * - Verify claims against primary evidence
     - *"Range-diff checked here, not from the paste"*; *"CI 36217297122 green"*; *"cancelled is not green"*

Two things it did **not** do: it never squash-merged (every merge line says *Cheng, for you in
person*), and it never decided the rules; it relayed Xi's decisions and asked when a rule was missing.
The overseer is this list, as a program, with the same two limits.

Position in the system
----------------------

.. code-block:: text

          GitHub (octocrab, scoped token)        git on machines (daemon jobs, overseer clearance)
                    ▲                                        ▲
                    │                                        │
   ┌────────────────┴────────────────────────────────────────┴──────────────┐
   │  fridica-overseer  (own process, own overseer.sqlite3)                  │
   │    WorkItem registry ── Planner (rules; a model for wording) ── Actor   │
   └────────────────┬───────────────────────────────────────────────────────┘
                    │ control socket: read threads, obligations, messages;
                    │ post through the outbox; delegate jobs; close obligations
   ┌────────────────┴───────────────────────────────────────────────────────┐
   │  fridica daemon (state.sqlite3)   actors ── parent ── supervisor ── outbox │
   └────────────────────────────────────────────────────────────────────────┘

The overseer never opens ``state.sqlite3``. It reads the daemon through the control API and it writes
through it: every Slack post it makes is an outbox row of kind ``overseer`` with the same idempotency
and ordering as any other post, and every git action it needs on a machine is a job with
``clearance = overseer`` scheduled by the daemon's supervisor on the daemon's slots. Its own database
holds the work items, the actions it took and the evidence it verified.

Work items
----------

A work item is Tianhao's F7 record: repo, PR or issue, exact head and tree, owner, status, evidence,
linked to any number of threads.

.. code-block:: rust

   pub struct WorkItem {
       pub id: WorkItemId,
       pub kind: WorkKind,              // PullRequest{repo, number} | Issue{..} | Check{name} | Review{of}
       pub repo: RepoRef,               // "chengcli/snapy"; the fork lives on the branch
       pub branch: Option<BranchRef>,   // {remote: "UCzhangxi/snapy", name: "xiz/spr10-output"}
       pub head: Option<Commit>,        // {sha, tree}: three people produced three shas for one tree
       pub base: Option<Commit>,
       pub owner: Person,               // one owner per item; the plain name used in threads
       pub state: WorkState,
       pub needs: Vec<Need>,            // SignOff{from, kind}, Ci, Rebase, Rerun{check}, Merge{by}
       pub evidence: Vec<Evidence>,     // verified facts with time: CiRun, RangeDiff, SignOff, Test
       pub threads: Vec<ThreadId>,
       pub due: Option<f64>,
       pub queue: Option<QueuePosition>,   // merge order within the campaign
       pub updated: f64,
   }

   pub enum WorkState {
       Draft,
       InReview,
       Blocked { on: Blocker },
       Behind { base_moved_to: Commit },
       Ready { since: f64 },
       WaitingForHuman { who: Person, what: HumanAction },
       Merged { sha: Commit, at: f64 },
       Closed { folded_into: Option<WorkItemId> },
   }

Items enter the registry in three ways: the owner or the human lead registers a campaign (a list of
PRs and branches, with owners and merge order) through ``fridica-overseer campaign add`` or the
dashboard; the daemon's parent, when it delegates a job that names a repository and branch, reports
it through the control API; and the F1 link follower reports every PR or issue link it resolves.
Items are keyed by ``(repo, number)`` or ``(repo, branch)``, so the three paths converge.

Clearance
---------

The overseer has more clearance than a worker and less than the owner. The clearance is a fixed
capability set; there is no way to widen it from a message, a PR body or a model output.

.. list-table:: Overseer clearance
   :header-rows: 1
   :widths: 40 12 48

   * - Capability
     - Allowed
     - Enforcement
   * - read Slack history, threads, obligations, jobs
     - yes
     - control API, read routes
   * - post to configured channels as the owner (kind ``overseer``)
     - yes
     - outbox; rate-limited per thread; never @-mentions in a blocked notice (F4)
   * - delegate git and check jobs on machines
     - yes
     - jobs carry ``clearance = overseer``; the worker gets the ``scoped-repo-fetch`` network allowance
       for the item's remotes and nothing else
   * - ``git fetch``, ``rebase``, ``cherry-pick``, ``range-diff``
     - yes
     - inside the job; read-only against upstream
   * - ``git push --force-with-lease`` to ``review/*`` and fork branches
     - yes
     - the job's git wrapper allows pushes only to refspecs matching ``[overseer] fork_remotes``;
       pushes use the owner's git credentials for the owner's own forks, or a per-repository deploy
       key the fork's owner added (see *Credentials* below)
   * - ``git push`` to an upstream repository (any branch)
     - **no**
     - the wrapper refuses; the overseer's PAT has ``contents: read`` on upstream, and branch
       protection restricts pushes to ``main`` to the owner
   * - open a PR, mark draft / ready, edit body status block and Review section, request reviewers,
       add labels, comment, close a PR it opened
     - yes
     - octocrab with the upstream token
   * - post a GitHub review (approve / request changes) on behalf of the owner
     - only ``SignOff`` with verified evidence (F9)
     - preconditions checked in code immediately before the call; the review body carries the
       evidence lines
   * - re-run a workflow
     - yes, once per head
     - ``actions: write`` on the fork; on upstream only if the token has it
   * - **squash-merge, merge, rebase-merge; delete a branch it did not create; edit branch protection**
     - **never**
     - no code path calls the merge API; the token lacks ``contents: write`` on upstream; the audit
       table records every API call so a violation would be visible
   * - resume / pause / instruct the owner's threads
     - resume and instruct, with an audit reason
     - control API with ``actor = overseer``; the daemon serializes it through the inbox
   * - change config, contract, tokens, the daemon's limits
     - **no**
     - the control API rejects settings changes from ``actor = overseer``

The "higher clearance" is therefore precisely: force-push with lease to review branches, PR
lifecycle short of merge, reviews with evidence, and steering the owner's threads. The "final squash
and merge" stays with a person, and GitHub is configured so that no token the overseer holds could do
it even if the code were wrong.

The campaign loop
-----------------

.. code-block:: rust

   pub async fn tick(&mut self) -> Result<()> {
       // GitHub state (the F1 client) for every open item; the daemon's obligations, jobs, threads
       self.refresh().await?;
       for item in self.registry.open_items() {
           let now = self.clock.now();
           let plan = planner::next(&item, &self.registry, &self.cfg, now);   // pure rules, no model
           for action in plan.actions {
               if let Err(refusal) = self.clearance.allows(&action, &self.ctx) {
                   self.audit.refused(&item, &action, refusal);
                   continue;
               }
               // a GitHub call, a job delegation, an outbox post, or an obligation close
               let outcome = self.perform(&item, action).await;
               self.registry.record(&item.id, &outcome).await?;
           }
       }
       if self.summary_due() { self.post_summary().await? }   // "only what changed since"
       Ok(())
   }

``planner::next`` is a table of rules, evaluated in order, each producing zero or more actions. The
model is not consulted for *what* to do; it is consulted for *wording* (summaries, restatements) and
for *interpretation* of a human's decision into a registry change, which the code then validates.

.. list-table:: Planner rules
   :header-rows: 1
   :widths: 36 64

   * - When
     - Actions
   * - the item's head moved (a push by anyone)
     - mark every sign-off not at the new tree ``stale``; cancel queued daemon jobs whose brief names the
       old sha (F9); recompute ``needs``
   * - base moved and the item is ``behind``
     - ``RebaseAndPush`` (a job on a machine that has the repo: fetch, rebase onto base, range-diff old
       vs new, push ``--force-with-lease`` to the fork branch), then ``PostRangeDiff`` in the item's
       thread; if the range-diff is all ``=`` and CI passes on the new head, carry sign-offs forward and
       record the evidence; any ``!`` → ``RequestReSign`` from every signer
   * - CI on the head is ``cancelled`` or ``failure`` and no rerun was tried for this head
     - ``RerunChecks``; on a second failure ``NotifyOwnerOfItem`` with the failing job names
   * - a needed sign-off has no evidence and its ask is older than the reminder grace
     - ``Remind`` (one per grace period, in the item's thread, plain names, with the exact line wanted)
   * - a needed sign-off's owner is marked away
     - ``Reassign`` if the campaign names a substitute, else ``AskLead``
   * - a thread linked to the item is paused, blocked or has an escalated obligation, and the item is
       waiting on that thread's owner
     - ``RestateInFreshThread`` (self-contained: repo, PR, head, tree, base, CI, what is wanted, due)
   * - all needs met, head on base (0 behind), CI green on the head, sign-offs at the head's tree
     - ``MarkReady`` (undraft, update body status block, put ``READY`` on the queue) and
       ``NotifyMerger`` (*"READY to squash-merge, for you in person"*, once; a reminder after the merge
       window passes)
   * - two items are ``READY`` and both touch the same files
     - keep only the first in merge order ``READY``; the other stays ``Draft`` with a note
   * - a human decision message is detected (*"from Xi"*, *"new rule"*, *"queue change"*) in a campaign
       thread
     - ``InterpretDecision``: the model proposes a registry diff (fold, reorder, new need kind); the
       code validates it (only known items, only known need kinds) and applies it; the diff is posted
       back as a confirmation line
   * - an ``ObligationDue`` on the owner's side names an item
     - nothing; the daemon's actor handles it. The overseer only reads obligations.

Evidence and sign-offs (F9)
---------------------------

The overseer treats every claim as untrusted until it verified it: a CI run id is resolved through
the API and its conclusion recorded; a range-diff is computed in a job, never copied from a message;
a sign-off is a GitHub review or a ``SIGN-OFF #n @ sha`` line whose sha the overseer resolves to a
tree. A ``SignOff`` action on behalf of the owner (when the owner's agent was the reviewer and its
worker's result says *approve*) runs these checks in code immediately before posting:

.. code-block:: rust

   fn signoff_preconditions(item: &WorkItem, ev: &WorkerResult, gh: &GithubState)
       -> Result<(), Refusal>
   {
       ensure!(gh.head.sha == ev.machine_state.commit, Refusal::HeadMoved);   // tested this head
       ensure!(gh.head.tree == item.head.tree, Refusal::TreeMismatch);
       ensure!(!gh.behind_base, Refusal::Behind);           // "a review of a sha behind main says so"
       ensure!(gh.checks.all_required(Conclusion::Success), Refusal::CiNotGreen);   // cancelled ≠ green
       ensure!(ev.validation.iter().all(|v| v.outcome == Passed), Refusal::TestsNotPassed);
       Ok(())
   }

The review body lists what was verified (head, tree, range-diff, CI run, tests with counts), which is
the campaign's rule that *"looks good" does not count*.

Interaction with the daemon's parent
------------------------------------

The parent's context gains an ``overseer`` block for threads linked to work items: the item's one-line
state, its needs and its due times, marked untrusted like ``github_state``. The parent does not plan
campaigns; it answers, delegates and reports, and it can *request* an overseer action through a new
action field (``overseer_requests: [{item, request: Rebase|Remind|Restate|Register}]``), which
becomes an ``OverseerRequest`` inbox row on the overseer's side. This is how *"can you rebase #222
after #223 merges"* said to the owner's agent turns into an overseer job without the parent having
git or GitHub tools.

Summaries and restatements
--------------------------

Summaries are generated from the registry by code (the table) and by the model (the prose: credit by
name, what changed since, one ask per person). The prompt receives the table and the delta as data;
the code checks that every sha, run id and count in the prose appears in the table before posting, and
drops any line that does not. Restatements use the same check. The header carries the configured time
zones. Cadence is ``[overseer] summary_interval``; a summary is skipped when the delta is empty.

Credentials for forks the owner does not own
--------------------------------------------

GitHub's fine-grained personal access tokens can be scoped only to repositories the token's owner
owns or to organizations they belong to; they cannot be granted access to another person's fork,
collaborator or not. A classic token with the ``repo`` scope could push to such a fork, but it would
also carry write access to every repository the owner can write to, upstream ``main`` included,
which defeats the token half of the no-merge guarantee. The overseer therefore has two push paths
and no third:

- **The owner's own repositories and forks.** The machine that runs the rebase job already holds the
  owner's git credentials (this is how workers push branches today); the wrapper limits the refspecs.
  The overseer's PAT covers the API side.
- **Another person's fork.** Only with a write-enabled *deploy key* for that one repository, generated
  by the owner (``fridica-overseer keys new <repo>``) and added by the fork's owner under the fork's
  settings. Deploy keys are per repository, so the key cannot reach anything else. Without one, the
  overseer does not push there: it posts the range-diff it computed and asks the fork's owner (or
  their overseer) to push, which is what the campaign did by hand.

The rule "review branches live on the contributor's fork" makes the second path rare: each owner's
overseer rebases its own owner's branches. The one-time GitHub setup for a repository owner is in
`Migration, testing and rollout <plan_>`_.

Failure and safety
------------------

- The overseer is stateless between ticks except for its database; killing it loses nothing. A tick
  that fails halfway leaves recorded actions recorded and unrecorded ones to be replanned; every
  performed action is idempotent by ``(item, action, head)``.
- It posts at most ``[overseer] max_posts_per_hour`` (default 12) and reminds at most once per item
  per ``reminder_grace`` (default 2 h). It never posts in a thread the owner paused by hand.
- ``fridica-overseer stop`` and the dashboard's kill switch stop it; the daemon keeps running.
- Everything it does is in its ``actions`` table and in the daemon's audit table with
  ``actor = overseer``; the daily report lists them.
- It runs under the owner's user account, with its own GitHub token in
  ``[overseer] github_token_env`` and the Slack tokens it never sees (it posts through the daemon).
