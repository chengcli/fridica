.. _attention:

The attention guarantee
=======================

Statement
---------

**Every mention of the owner by a person ends in a reply, a posted reason, or an escalation the owner
can see; every explicit ask with a due time is tracked until it is closed.** The guarantee is enforced
by code, not by the contract: the gate cannot return a verdict that discards a mention, and an
obligation row exists for the mention before the actor decides anything about it.

This section replaces the parts of ``threads/policy.py`` that the evidence section found wanting. It
keeps every other rule of 0.3 (owner posts never trigger, generated peer posts are ignored unless
addressed, cooldowns, triage).

Obligations
-----------

An obligation is a row that says "we owe this thread something". It is created inside the same
transaction as the message that caused it, so a crash cannot lose it.

.. list-table:: Obligation kinds
   :header-rows: 1
   :widths: 22 46 32

   * - Kind
     - Created when
     - Closed when
   * - ``Mention``
     - a message by a person (not the owner, not our own echo) contains ``<@owner>``
     - a post of ours in that thread after the message is sent (``answered``); or the parent declines
       with a reason that is posted or recorded (``declined``); or the owner closes it
   * - ``Ask``
     - the parent's action lists an ``ask`` with a due time found in a message that addressed us
       (*"PR 7 CUDA gate … due 05:00 PT"*)
     - the parent marks it answered in a later action, a job's result covers it, or the owner closes it
   * - ``Waiting``
     - we replied with status ``waiting`` and named a person
     - that person posts in the thread (then the reply obligation is theirs, and ours is a new
       ``Mention``-like one to answer them, created by the existing "answering our question" rule)
   * - ``JobReport``
     - a job is queued for this thread
     - the job's result is composed into a reply, or the interruption is reported
   * - ``OwnerInstruction``
     - the owner instructs a thread from the dashboard or MCP
     - the actor applies it

.. code-block:: rust

   pub struct Obligation {
       pub id: ObligationId,
       pub thread: ThreadId,
       pub kind: ObligationKind,
       pub source: ObligationSource,   // Message{event, sender} | Job{id} | Owner
       pub summary: String,            // one line, plain names, ≤ 300 chars; dashboard and report
       pub created: f64,
       pub due: Option<f64>,           // from the ask, in the owner's timezone; None for mentions
       pub state: ObligationState,
   }
   pub enum ObligationState {
       Open,
       Escalated { at: f64, to: EscalationTarget },   // owner attention, report, MCP; never a post
       Answered { by: OutboxId },
       Declined { reason: String, posted: Option<OutboxId> },
       Closed { by: Actor },                          // the owner, or the overseer with a reason
       Expired,                                       // the thread was archived or cleaned
   }

The gate, revised
-----------------

The gate stays a pure function. Two things change: a mention is never observed away, and the
automatic pause is gone. ``ThreadControl::Paused`` now has exactly one source, the owner (dashboard,
CLI, MCP, or the overseer with a reason), and the loop protections that used to pause a thread are
replaced by the throttles and signals of the next section.

.. list-table:: The revised gate (first matching row wins)
   :header-rows: 1
   :widths: 44 22 34

   * - Condition
     - Verdict
     - Note
   * - observe-only mode
     - observe
     - unchanged
   * - the owner wrote (our echo → ignore)
     - observe / ignore
     - unchanged
   * - control is paused by the owner, closed, archived or cleaned
     - observe
     - unchanged; a mention there still creates an obligation, which escalates at once, so an
       owner who paused a thread by hand sees what it is missing
   * - sent before ``reset_at`` and not resumed
     - observe
     - unchanged
   * - generated peer post, debrief root
     - ignore
     - unchanged
   * - generated peer post, finished, not addressed to us, not waiting
     - ignore
     - unchanged
   * - status blocked, mentioned
     - notice, then respond if the parent classifies the mention as a new instruction
     - **changed** (R6): the F4 notice is posted once; a mention that carries a new ask reopens the
       thread as ``working``
   * - mentioned, or answering our question
     - respond
     - unchanged in wording; **no longer preceded by a pause check** (R1, R2). Whether the reply
       goes out now or is throttled is decided by the actor, not the gate
   * - generated, not addressed
     - ignore
     - unchanged
   * - follow-up in a thread we are part of
     - triage
     - unchanged
   * - unaddressed, general messages on, not cooling
     - triage
     - unchanged
   * - otherwise
     - observe
     - unchanged

.. code-block:: rust

   pub fn gate(msg: &Message, s: &ThreadSession, r: &Rules) -> Verdict {
       if r.observe_only { return Verdict::observe("observe-only mode") }
       if msg.sender == r.owner {
           return if msg.generated() { Verdict::ignore("our own post") }
                  else { Verdict::observe("the owner wrote") };
       }
       let mentioned = msg.mentions(&r.owner) || r.resumed;
       if !matches!(s.control, Active) {        // owner-paused, closed, archived, cleaned
           return Verdict::observe(format!("thread is {}", s.control))
               .with_obligation(mentioned).escalate_if(mentioned);
       }
       if !r.resumed && s.reset_at.map_or(false, |t| msg.ts <= t) {
           return Verdict::observe("sent before the thread was resumed");
       }
       if let Some(meta) = &msg.meta {                    // generated by some owner's Fridica
           if meta.kind == Kind::DebriefRoot { return Verdict::ignore("another agent's debrief") }
           if matches!(meta.status, Complete | Blocked) && !(mentioned || s.waiting()) {
               return Verdict::ignore("another agent finished its reply");
           }
           if !(mentioned || s.waiting()) {
               return Verdict::ignore("another agent's message not addressed to us");
           }
       }
       if s.status == Blocked {
           return if mentioned { Verdict::notice_then_maybe_respond() }
                  else { Verdict::observe("blocked thread") };
       }
       if mentioned || s.waiting() {
           return Verdict::respond(if mentioned { "addressed" } else { "answering our question" });
       }
       if msg.generated() { return Verdict::ignore("another agent's message") }
       if s.turns > 0 { return Verdict::triage("follow-up in a thread we are part of") }
       if r.general_messages && !r.cooling { return Verdict::triage("unaddressed message") }
       Verdict::observe("not addressed")
   }

Loop protection without a pause
-------------------------------

0.3 had one loop protection, the "three turns" rule: three consecutive quiet turns (no reply, an
identical reply, or an ack), or three consecutive ``waiting`` replies, paused the thread until a
person resumed it. It was meant to stop two Fridicas from talking forever. In the last run it fired
18 times, never on an agent loop, and every time on a thread where the quiet turns were correct
silences on other people's traffic; the pauses then swallowed 89 mentions. The rule detects the wrong
thing and responds the wrong way, so 0.4 drops it and replaces it with three guards that each aim at
one cause.

.. list-table:: Loop guards in 0.4
   :header-rows: 1
   :widths: 22 40 38

   * - Guard
     - Stops
     - Mechanism
   * - **No identical resend** (F5)
     - a parent that repeats itself
     - a reply with the same text and status as the thread's last one, and nothing new, is not
       sent; exceptions as in F5 (mention, explicit repost, correction, owner instruction, worker
       result). The verdict is recorded as ``suppressed_repeat``.
   * - **Echo ceiling**
     - two Fridicas answering each other
     - replies whose trigger carries Fridica metadata (a post by some owner's Fridica) count
       against ``max_echo_replies_per_hour`` per thread (default 10). Over the ceiling, the reply is
       *throttled*: the inbox item waits until the trailing hour has room, the obligation is
       escalated with reason ``throttled``, and the owner is told once per hour per thread. The
       peer's turn counter in the metadata is still honoured.
   * - **Reply ceiling**
     - a runaway with an agent that posts as a person (Xi's lead session, Tianhao's bot), or any
       other fast loop the metadata cannot see
     - every reply to a non-owner trigger counts against ``max_replies_per_hour`` per thread
       (default 20); same throttle-and-escalate behaviour. Owner instructions, control items and
       worker results are exempt.

Throttling defers, it never drops. A throttled item stays ``pending`` in the inbox with a
``not_before`` time; the actor takes it when the window has room; the obligation created for the
mention is open the whole time and shows as escalated. Two Fridicas that mention each other
therefore exchange at most six messages an hour per thread, each of them a real reply, and the
owner knows. A loop with an unmarked agent is capped at twenty an hour and is visible within the
hour. The 0.3 rule capped both at three and hid them for days.

.. code-block:: rust

   // in the actor, before a decide call for a non-owner trigger
   fn throttle(&self, msg: &Message, now: f64) -> Option<f64> {         // Some(not_before) = wait
       let att = &self.rt.cfg.attention;
       let window = now - 3600.0;
       let recent = self.rt.store.replies_since(&self.id, window)?;       // (count, echo_count, oldest)
       if msg.generated() && recent.echo_count >= att.max_echo_replies_per_hour {
           return Some(recent.oldest_echo + 3600.0);
       }
       if recent.count >= att.max_replies_per_hour { return Some(recent.oldest + 3600.0) }
       None
   }

Streaks become signals
----------------------

The two counters of 0.3 survive as *signals*, not gates. ``wait_streak`` (consecutive ``waiting``
replies) and ``quiet_streak`` (consecutive replies that were acks or suppressed repeats) are kept in
the session exactly as before, with one change to what counts (R3): a deliberate non-reply to a
message that did not address us leaves ``quiet_streak`` unchanged. When either streak reaches
``streak_signal`` (default 3) the sweeper raises an escalation on the thread's newest open
obligation, or creates a ``Signal`` obligation if none is open, with the text the owner sees:
*"three clarifying questions in a row"* or *"three replies without new content"*. Nothing is paused
and nothing is delayed; the owner decides whether to pause, instruct or leave it.

.. list-table:: What counts toward ``quiet_streak``
   :header-rows: 1
   :widths: 60 20 20

   * - Turn
     - 0.3
     - 0.4
   * - reply sent, different text, not an ack
     - resets
     - resets
   * - reply identical to the last (suppressed by F5)
     - counts
     - counts
   * - reply sent with note kind ``ack``
     - counts
     - counts
   * - delegation made
     - resets
     - resets
   * - no reply, no delegation, the message **addressed us**
     - counts
     - counts, and the obligation is ``declined`` with the parent's reason
   * - no reply, no delegation, the message **did not address us**
     - counts
     - **neutral**
   * - streak reaches 3
     - **pause**
     - **escalation signal to the owner**

The owner's pause
-----------------

``ThreadControl::Paused`` remains, with one source: a person or the overseer chose it. It behaves as
in 0.3 (no replies, no triage, no delegation; ``resume`` clears it and sets ``reset_at``), with one
addition: a mention in an owner-paused thread creates an obligation that escalates immediately, so
the pause never hides an ask from the person who paused it.

The sweeper
-----------

The ``attention`` module runs one task in the daemon:

.. code-block:: rust

   pub async fn sweep(rt: &Runtime) -> Result<()> {
       let now = rt.clock.now();
       let grace = rt.cfg.attention.mention_grace;
       for o in rt.store.obligations_open_past(now - grace).await? {            // R4
           rt.store.escalate(&o.id, EscalationTarget::Owner, now).await?;      // dashboard, report, MCP
           rt.bus.ring(Doorbell::Attention);
       }
       for o in rt.store.obligations_due_before(now).await? {                   // asks with due times
           // the actor gets one decide call whose context says which ask is due
           let item = InboxItem::ObligationDue { obligation: o.id };
           rt.store.enqueue(&o.thread, item).await?;
       }
       for t in rt.store.threads_with_streak(rt.cfg.attention.streak_signal).await? {   // signals
           rt.store.signal(&t, Signal::from_streaks(&t), now).await?;            // once per streak
       }
       for t in rt.store.threads_throttled_until_before(now).await? {           // ceilings freed
           rt.bus.ring_thread(&t);                                              // the actor retries
       }
       Ok(())
   }

An ``ObligationDue`` item gives the actor one decide call whose context says which ask is due; the
parent may report status, ask for more time (``deferred``), or say it is done. This is the mechanism
that answers *"A status check on your three items. Two are past due"* before the check is posted
rather than after.

Escalation never posts to the channel. It changes what the owner sees: the dashboard's attention
view, ``fridica obligations``, the daily report and the MCP ``attention`` tool all list escalated
obligations first, with the thread link and the one-line summary. The owner closes an obligation
from any of those, or answers in Slack, which closes it through the normal path.

What a replay of the last run would do differently
--------------------------------------------------

With the revised gate and guards, the corpus replays as follows (these are the parity exceptions of
rule P5 for this section, each with a test):

- none of the 17 no-progress pauses and the 1 wait-loop pause happens; 89 mentions in what were
  paused threads become decide calls. None is throttled: no thread in the corpus received more than
  20 replies in an hour, and no trigger with Fridica metadata mentioned the owner in those threads.
- 3 threads reach a ``quiet_streak`` of 3 (acks and repeats) and 1 a ``wait_streak`` of 3; each
  raises one signal to the owner and keeps answering.
- 18 mentions in blocked threads get the F4 notice once and, where the parent finds a new instruction
  (the *"Redone with snapy's pre-commit style…"* and *"SIGN-OFF #214 … approve"* posts are examples),
  reopen the thread.
- 29 asks with due times become ``Ask`` obligations with ``due``; the sweeper drives a status turn at
  each due time.
- 12 deliberate ``send: false`` decisions on mentions remain silent, but their obligations are
  ``declined`` with the parent's stated reason and show in the report as such.

Contract changes
----------------

Two additions to the packaged ``contract.md``, under ``## Replies``:

- *When the conversation data lists open obligations for this thread, account for each one: mark it
  answered if this reply answers it, declined with a one-line reason if it needs no reply, or deferred
  with a time if it will be answered later. Never leave an obligation unaccounted for.*
- *When a message that addresses you names a due time, list it under asks with the due time as
  written.*

The contract still decides *what* to say. The code decides that something is said.
