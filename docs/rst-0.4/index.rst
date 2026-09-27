==============================================
Fridica 0.4: Design Document for the Rust Core
==============================================

--------------------------------------------------------------------------------------------------
A Rust daemon, an attention guarantee for mentions, a standing overseer, and daily channel reports
--------------------------------------------------------------------------------------------------

:Version: 0.4 design, revision 1 (2026-09-27); targets schema v5, config v2
:Baseline: Python 0.3.3 (``5249e2e``) plus the unmerged review branches F1–F6 (PRs #26–#31) and the ``fix-exit-status`` branch
:Evidence: the 2026-09-24 21:27 to 2026-09-27 10:18 run in ``state.sqlite3`` (1,831 messages, 1,533 inbox items, 232 jobs, 606 posts) and the Slack campaign threads of that run
:Status: design only; nothing in this document is implemented

.. topic:: Abstract
   :class: abstract

   Fridica 0.3 turned a Slack account into an agent that answers in threads and delegates work to
   Claude Code and Codex workers on the owner's machines. It works, and the last campaign run showed
   both what it does well and where it fails. This document designs the next overhaul, 0.4, which
   rewrites the daemon's core in Rust while keeping the semantics that 0.3 settled, keeping the
   JavaScript dashboard, and keeping ``pip install fridica`` as the way to get it. Four things are new.
   First, the core becomes one Rust crate on Tokio, with typed state machines for threads,
   workers, deliveries and obligations, and Serde-typed backend protocols; the Python tree becomes the
   executable specification the port must match on a replay corpus. Second, an *attention guarantee*:
   every mention of the owner ends in a reply, a visible reason, or an escalation to the owner. In the
   last run 98 of 483 mentions by other people ended in silence, 71 of them because a loop-protection
   pause swallowed them; the automatic pause is removed in favour of rate ceilings that defer and
   escalate, and the blocked and no-progress rules are redesigned so that this cannot happen. Third, a separate standing *overseer* process, modelled on the PR-lead session Xi ran by hand
   during the campaign: it tracks work items across repositories and threads, unblocks them (rebases,
   force-pushes with lease to review branches, re-sign requests, reminders, fresh restatements), and
   moves the merge queue forward under a higher but bounded clearance that stops short of the final
   squash-merge. Fourth, a daily report per channel, stored as Markdown and served through an MCP
   server so that Claude Desktop and Codex Desktop can read it and act on it. The document ends with
   the schema, the migration and rollout plan, the test strategy, and skeleton code for every new
   component.

.. header::

   .. class:: headertext

   Fridica 0.4 design document

.. footer::

   .. class:: footertext

   Page ###Page###

.. raw:: pdf

   PageBreak mainPage

.. contents:: Contents
   :depth: 2

.. sectnum::
   :depth: 2

.. raw:: pdf

   PageBreak

.. include:: 01_goals.rst
.. include:: 02_baseline.rst
.. include:: 03_evidence.rst
.. include:: 04_rust_core.rst
.. include:: 05_attention.rst
.. include:: 06_overseer.rst
.. include:: 07_reporting_mcp.rst
.. include:: 08_data_model.rst
.. include:: 09_security.rst
.. include:: 10_plan.rst

.. raw:: pdf

   PageBreak

.. include:: 11_appendix_code.rst
