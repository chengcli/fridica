.. _evidence:

Evidence from the last run
==========================

The daemon that ran 0.3.3 from 2026-09-25 08:51 (started after a 2-hour upgrade window) kept a
database that also holds the messages from the evening before. This section reads that database for
the one question the design has to answer: **which mentions and tasks were forgotten, and why**. All
numbers are counts over the whole database at the snapshot time (2026-09-27 10:18 ET); people other
than the owner are named by role.

The run at a glance
-------------------

.. list-table:: The 2026-09-24 21:27 to 2026-09-27 10:18 run
   :header-rows: 1
   :widths: 40 60

   * - Quantity
     - Value
   * - Messages stored (one channel, five senders)
     - 1,831; 616 by the owner, 1,213 by four other people or their agents
   * - Inbox items
     - 1,533 (1,288 messages, 227 worker results, 12 debriefs, 4 worker interruptions, 2 controls); all ``done``
   * - Parent calls
     - 441 decide (2 errors), 316 triage, 10 debrief; median latency 22 s for decide and 5.5 s for triage
   * - Jobs
     - 232: 210 done, 18 interrupted, 2 failed, 1 cancelled, 1 running
   * - Posts
     - 606, all ``sent``: 364 replies, 159 reports, 62 uploads, 11 notices, 10 debriefs
   * - Threads
     - 142: 59 new, 63 complete, 4 waiting, 12 working, 4 blocked; 18 paused
   * - Owner actions
     - 2 thread resumes in 2.5 days

Nothing was lost in the durability sense: every inbox item was processed, every outbox row was sent,
no post is ambiguous or blocked. The losses are decisions, not crashes.

Mentions of the owner
---------------------

Other people mentioned the owner 483 times. The gate's verdict on those messages:

.. list-table:: Verdicts on the 483 mentions by others
   :header-rows: 1
   :widths: 40 15 45

   * - Verdict
     - Count
     - Outcome
   * - ``respond: addressed``
     - 376
     - a decide call; 364 led to a post, 12 were deliberate ``send: false`` decisions (hourly summaries
       the lead said needed no reply, a holiday greeting that asked agents not to answer). These are
       correct.
   * - ``observe: thread is paused``
     - 89
     - **no model call, no post, nobody told**
   * - ``notice: blocked thread``
     - 18
     - a fixed notice, at most once per blocked period; 11 notices were sent for 18 mentions

Of the 107 mentions in paused or blocked threads, 98 were never followed by any post of ours in that
thread. 29 of the 107 carried an explicit ask, a due time, a "past due" status check or a "ready for
your squash-merge" line. Examples, abridged (the lead's session, addressing the owner's agent in a
paused thread): *"For your PR 7 re-sign: the patch is committed locally as 5c6c494…"*, *"Post the second
PR 3 replay tree now"*, *"A status check (17:52 PT) on your three items. Two are past due"*, *"#222 is
READY for your squash-merge"*, *"#227 re-sign, gentle reminder"*. None of them reached a model.

The lead worked around it by hand: 21 fresh top-level threads that mention the owner were opened to
restate asks "in case the long ones got compacted on your side", and 9 reminders or check-ins were
posted. Xi's own analysis of the campaign counted 46 fresh-thread restarts, 69 reminders and 78 state
corrections across all participants; a good share of the ones aimed at this owner are explained by
the mechanism below.

Why the threads were paused
---------------------------

18 threads are paused: 17 with *"3 turns without progress; review before continuing."* and 1 with
*"3 consecutive replies needed more information"*. They have been paused for between 9 and 59 hours
(median 36 h). Only two threads were ever resumed by the owner.

The no-progress counter (``threads/policy.py``, ``advance()``) increments on every turn that is
*quiet*: a decision not to reply and not to delegate, a reply identical to the last one, or a reply
whose note kind is ``ack``. Three in a row pause the thread. In the campaign channel, a thread is a
stream of status posts from four agents; most of them do not address this owner, and the correct
decision is silence. Three correct silences in a row (or one ack and two silences) paused the thread,
and from then on ``policy.gate`` returned ``observe`` for everything, mentions included:

.. code-block:: python

   if session.control != "active":
       return Verdict("observe", f"thread is {session.control}")   # before the mention check

The last three decide calls before each pause show the pattern: 15 of 51 were ``send: false``; the
rest were replies with ``status: complete`` that repeated or acknowledged. The loop protection is
doing what it was written to do, stop two agents from talking forever, but it cannot tell an
agent-to-agent echo from a human's direct instruction, and once it fires it stays fired until a
person opens the dashboard.

The blocked-thread rule has the same shape at a smaller scale: a mention in a blocked thread gets one
notice per blocked period, and every further mention, including one that answers the blocker, is
observed. F4 makes the notice actionable; it does not make the thread listen again.

Jobs
----

18 jobs ended ``interrupted``: 4 by the daemon stopping (the upgrade window), 1 by an explicit
interrupt, 1 without a recorded reason, and 12 with ``job failed: error_during_execution`` from the
Claude backend. Only 4
``worker_interrupted`` inbox rows exist, so most backend failures reached the thread as a failed
result rather than as an interruption, and none of them was retried. One job failed because the
backend's safety classifier refused the brief; one because SSH to a machine did not connect. Two
observations for the design: a backend failure is a distinct state that should be retried once with
the same session before it is reported (``auto_resume`` covers only daemon restarts today), and a
brief refused by a classifier should be reported as such, with the text of the refusal, rather than as
a generic failure.

Requirements derived from the evidence
--------------------------------------

.. list-table:: Requirements
   :header-rows: 1
   :widths: 8 92

   * - Id
     - Requirement
   * - R1
     - A mention of the owner by a person, in any thread state, is answered by a model call or by a
       posted reason. Silence is never the outcome of a gate rule alone.
   * - R2
     - No rule pauses a thread automatically. Loop protection is expressed as per-thread rate
       ceilings on replies (a low one for triggers that carry Fridica metadata, a higher one for
       everything else) that defer and escalate rather than drop, plus the no-identical-resend rule.
   * - R3
     - A deliberate non-reply to a message that did not address the owner is not "no progress".
   * - R4
     - Every mention and every explicit ask with a due time is an *obligation* with a state, visible
       to the owner until it is closed, and escalated when it is overdue.
   * - R5
     - A pause has exactly one source, the owner; a mention in an owner-paused thread is escalated
       to the owner at once rather than dropped.
   * - R6
     - A blocked thread reopens when a mention carries a new instruction; the F4 notice is posted once
       and the obligation stays open.
   * - R7
     - A backend execution error is retried once, continuing the session, before it is reported; a
       classifier refusal is reported verbatim.
   * - R8
     - The owner can see, without opening Slack, what is open, overdue, paused, blocked and waiting
       for them: in the dashboard, on the command line, in the daily report and through MCP.
