.. include:: generated/numbers.rst

=========================
Fridica: Design Document
=========================

----------------------------------------------------------------------------------------------
A Slack-native agent that coordinates Claude and Codex workers across machines
----------------------------------------------------------------------------------------------

:Version: overhaul (PR #22 and later), schema v\ |schema_version|
:Data snapshot: |snapshot|
:Current design: |new_start| to |new_end| (|new_hours| hours)
:Legacy design: |old_start| to |old_end| (|old_days| days)
:Code: |lines| lines of Python in ``src/fridica``; |test_functions| test functions in |test_files| files

.. topic:: Abstract
   :class: abstract

   Fridica lets one person's Slack account take part in conversations the way the person would: it reads
   channels, decides when to answer, and when work needs tools it delegates that work to Claude or Codex
   agents running on the person's own machines, locally or over SSH, next to the code, data and GPUs.
   This document describes the design introduced by the large overhaul (PR #22) and refined since.
   It is organized by concern: how Slack concepts map onto runtime objects, the layers of the code,
   how context is bounded at every hop, how concurrency and durability are achieved, how
   credentials and machines are protected, and how Linux namespaces, bubblewrap, socat and seccomp
   confine each worker to its folder. Every number is computed from Fridica's own SQLite state
   database by the scripts in ``docs/scripts``; identities are anonymized and no message text is used.
   An appendix compares the overhaul with the previous design using the preserved legacy database.

.. header::

   .. class:: headertext

   Fridica design document

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

.. include:: 01_overview.rst
.. include:: 02_slack_mapping.rst
.. include:: 03_layers.rst
.. include:: 04_context_management.rst
.. include:: 05_concurrency_durability.rst
.. include:: 06_security.rst
.. include:: 07_sandboxing.rst
.. include:: 08_operations.rst

.. raw:: pdf

   PageBreak

.. include:: 09_appendix_comparison.rst
