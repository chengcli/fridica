"""Durable state: one SQLite database owned by the daemon."""

from __future__ import annotations

import os
from pathlib import Path

from .approvals import Approvals
from .db import Database
from .messages import Inbox, Messages
from .notes import Audit, Notes, ParentTurns
from .outbox import Outbox
from .threads import Cooldowns, StaleSession, Threads
from .work import Artifacts, Jobs, Workers

__all__ = ["Store", "StaleSession"]


class Store:
    def __init__(self, path: Path, *, lock: bool = True, readonly: bool = False):
        self.db = Database(path, lock=lock, readonly=readonly)
        self.messages = Messages(self.db)
        self.inbox = Inbox(self.db)
        self.threads = Threads(self.db)
        self.cooldowns = Cooldowns(self.db)
        self.workers = Workers(self.db)
        self.jobs = Jobs(self.db)
        self.artifacts = Artifacts(self.db)
        self.outbox = Outbox(self.db)
        self.approvals = Approvals(self.db)
        self.notes = Notes(self.db)
        self.audit = Audit(self.db)
        self.parent_turns = ParentTurns(self.db)

    def transaction(self):
        return self.db.transaction()

    def close(self) -> None:
        self.db.close()

    def recover(self, now: float) -> dict[str, int]:
        """Settle work a previous daemon left in flight; run once before connecting to Slack.

        Nothing ambiguous is replayed: a post that may have reached Slack becomes
        ``ambiguous``, and a job that was running becomes ``interrupted`` with an inbox
        event so its thread can decide what to tell people.
        """
        counts = {}
        with self.transaction():
            db = self.db
            # A parent turn is recorded in the same transaction as all of its effects, so its
            # presence means the item was fully handled.
            counts["inbox_done"] = db.execute(
                "UPDATE thread_inbox SET state='done' WHERE state='processing'"
                " AND id IN (SELECT inbox_id FROM parent_turns)").rowcount
            counts["inbox_retried"] = db.execute(
                "UPDATE thread_inbox SET state='pending' WHERE state='processing'").rowcount
            counts["outbox_ambiguous"] = db.execute(
                "UPDATE outbox SET state='ambiguous', error='daemon stopped while sending' WHERE state='sending'").rowcount
            interrupted = db.all("SELECT id, worker_id, session_id FROM jobs WHERE status='running'")
            for row in interrupted:
                db.execute("UPDATE jobs SET status='interrupted', error='daemon stopped', finished_at=? WHERE id=?",
                           (now, row["id"]))
                db.execute("UPDATE workers SET status='lost', updated=? WHERE id=?", (now, row["worker_id"]))
                self.inbox.add(row["session_id"], "worker_interrupted", now, ref=row["id"])
            counts["jobs_interrupted"] = len(interrupted)
            db.execute("UPDATE workers SET status='idle', updated=? WHERE status IN ('running','queued','awaiting_approval')",
                       (now,))
            counts["approvals_expired"] = db.execute(
                "UPDATE approvals SET status='expired', decided_by='restart', decided_at=? WHERE status='pending'",
                (now,)).rowcount
        return counts

    def heartbeat(self, *, started_at: float, now: float, slack_status: str, observe_only: bool,
                  control_socket: str = "", fingerprint: str = "") -> None:
        self.db.execute(
            "INSERT OR REPLACE INTO runtime (id, pid, started_at, heartbeat_at, slack_status, observe_only,"
            " control_socket, config_fingerprint) VALUES (1,?,?,?,?,?,?,?)",
            (os.getpid(), started_at, now, slack_status, int(observe_only), control_socket, fingerprint))

    def runtime(self) -> dict | None:
        row = self.db.one("SELECT * FROM runtime WHERE id=1")
        return dict(row) if row else None
