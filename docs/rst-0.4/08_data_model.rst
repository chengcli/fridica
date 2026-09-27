.. _data-model:

Data model and schema v5
========================

Schema v5 is the first Rust schema. It is a superset of v4 (0.3.3 plus F3): no existing column is
removed or renamed, so a v5 database can still be read by the 0.3 documentation scripts, and the
migration is additive. ``src/store/schema.rs`` is the only module that runs DDL, as
``store/schema.py`` was.

.. list-table:: Tables in v5
   :header-rows: 1
   :widths: 20 12 68

   * - Table
     - Status
     - Columns (new ones in bold)
   * - ``meta``, ``runtime``, ``cooldowns``, ``audit``, ``notes``, ``artifacts``, ``approvals``
     - unchanged
     - ``audit.actor`` gains the values ``system``, ``overseer``, ``owner-desktop``
   * - ``messages``
     - unchanged (v4)
     - ``attachments_json`` from F3; **``mentions_owner INTEGER``** (computed at intake, indexed, so
       the sweeper and the report never scan text)
   * - ``threads``
     - extended
     - **``control_json TEXT``** (the serialized ``ThreadControl`` with who paused it and why; the
       ``control`` and ``pause_reason`` columns stay in sync for old readers); ``no_progress`` is
       read as ``quiet_streak``; **``throttled_until REAL``**
   * - ``thread_inbox``
     - unchanged
     - ``kind`` gains ``obligation_due`` and ``overseer_request``
   * - ``outbox``
     - unchanged
     - ``kind`` gains ``overseer`` and ``report``; **``answers_json TEXT``** (obligation ids this post
       answers)
   * - ``jobs``
     - extended
     - **``clearance TEXT``** (``worker`` | ``overseer``), **``stale_for TEXT``** (a sha; set when the
       work item's head moved and the job should be cancelled if still queued), **``retry_of TEXT``**
       (R7)
   * - ``workers``
     - unchanged
     - ``status`` keeps its values; the typed state lives in memory and is projected to the column
   * - ``parent_turns``
     - unchanged
     - ``call`` gains ``obligation_due``, ``summary``
   * - **``obligations``**
     - new
     - ``id, session_id, kind, source_json, summary, created, due, state, state_json, updated``
   * - **``work_items``**
     - new (mirror)
     - ``id, kind, repo, number, branch_json, head_sha, head_tree, base_sha, owner, state, state_json,
       needs_json, due, queue_pos, updated``; written only through the control API by the overseer
   * - **``reports``**
     - new
     - ``channel, day, data_json, markdown, created``; primary key ``(channel, day)``
   * - **``channel_watermarks``**
     - new (was ``meta`` keys in F2)
     - ``channel, last_complete_pass, pinned``

The overseer's own database (``overseer.sqlite3``, beside the state database) holds:

.. list-table:: Overseer tables
   :header-rows: 1
   :widths: 22 78

   * - Table
     - Columns
   * - ``campaigns``
     - ``id, name, repos_json, merge_windows_json, people_json (names, roles, away), created``
   * - ``work_items``
     - the full ``WorkItem`` (the daemon's table is a projection of this one)
   * - ``evidence``
     - ``id, item_id, kind, data_json, verified_at, verified_by`` (``api`` | ``job:<id>``)
   * - ``actions``
     - ``id, item_id, action, head_sha, state (planned|done|failed|refused), detail_json, created, finished``
   * - ``decisions``
     - ``id, source_message, proposed_diff_json, applied_diff_json, applied_at`` (human decisions
       interpreted into registry changes)
   * - ``summaries``
     - ``id, campaign_id, delta_json, text, posted_as (outbox id), created``

Migration v4 → v5
-----------------

``fridica migrate`` (also run by ``fridica start`` when it finds a v4 database, after taking a
backup copy through the SQLite backup API):

1. ``ALTER TABLE`` additions listed above, with defaults; ``mentions_owner`` back-filled from
   ``messages.text`` for the owner in ``meta``.
2. ``threads.control_json`` derived from ``control`` and ``pause_reason``: a thread paused by the
   0.3 loop rules (its ``pause_reason`` is one of the two rule texts) is resumed at migration
   through a ``system`` control item the actor handles, so the 18 threads paused today are
   answering again after the first 0.4 start and the resumption is in the audit table; a thread the
   owner paused stays paused with ``by = owner``.
3. Obligations are **not** reconstructed for old mentions; the report of the first day lists the
   count of pre-migration unanswered mentions instead, and ``fridica obligations --backfill`` can
   create them on request.
4. F2's watermark keys move from ``meta`` to ``channel_watermarks``.
5. ``meta.schema_version = 5``.

A v5 database is refused by 0.3 (``schema_version`` too new), which is the same protection 0.3 has
against 0.2.
