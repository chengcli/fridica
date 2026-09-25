"""The durable outbox: every Slack post, idempotent by key."""

from __future__ import annotations

from ..core.models import OutboxItem
from . import codec
from .db import Database

DEPENDENTS = ("WITH RECURSIVE dependents(key) AS (SELECT idem_key FROM outbox WHERE id=?"
              " UNION SELECT o.idem_key FROM outbox o JOIN dependents d ON o.after=d.key)")


class Outbox:
    def __init__(self, db: Database):
        self.db = db

    def enqueue(self, item: OutboxItem, now: float) -> bool:
        """Queue a post; a repeated idempotency key is ignored. Returns whether it was new.

        A prerequisite (``after``) must already be queued, so dependencies always point
        backwards and can never deadlock.
        """
        if item.after and self.get(item.after) is None:
            raise ValueError(f"outbox prerequisite {item.after} is not queued")
        cursor = self.db.execute(
            "INSERT OR IGNORE INTO outbox (idem_key, session_id, kind, channel, thread_ts, text, meta_json, filename,"
            " blob, after, created) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            (item.idem_key, item.session_id, item.kind, item.channel, item.thread_ts, item.text,
             codec.meta_json(item.meta), item.filename, item.blob, item.after, now),
        )
        return cursor.rowcount == 1

    def get(self, idem_key: str) -> OutboxItem | None:
        row = self.db.one("SELECT * FROM outbox WHERE idem_key=?", (idem_key,))
        return codec.outbox(row) if row else None

    def by_id(self, item_id: int) -> OutboxItem | None:
        row = self.db.one("SELECT * FROM outbox WHERE id=?", (item_id,))
        return codec.outbox(row) if row else None

    def ready(self, now: float, *, limit: int = 20) -> list[OutboxItem]:
        """Pending posts that are due, whose prerequisite was sent, and that are first in their thread's queue.

        A post whose prerequisite failed or became ambiguous is moved to ``blocked`` by
        ``fail`` and waits for an operator to retry the prerequisite.
        """
        rows = self.db.all(
            "SELECT o.* FROM outbox o WHERE o.state='pending' AND o.retry_at<=?"
            " AND (o.after='' OR EXISTS (SELECT 1 FROM outbox p WHERE p.idem_key=o.after AND p.state='sent'))"
            " AND NOT EXISTS (SELECT 1 FROM outbox e WHERE e.channel=o.channel AND e.id<o.id"
            "   AND COALESCE(e.thread_ts, e.idem_key)=COALESCE(o.thread_ts, o.idem_key)"
            "   AND e.state IN ('pending','sending'))"
            " ORDER BY o.id LIMIT ?",
            (now, limit),
        )
        return [codec.outbox(row) for row in rows]

    def claim(self, item_id: int) -> bool:
        cursor = self.db.execute("UPDATE outbox SET state='sending', attempts=attempts+1 WHERE id=? AND state='pending'",
                                 (item_id,))
        return cursor.rowcount == 1

    def sent(self, item_id: int, ts: str) -> None:
        self.db.execute("UPDATE outbox SET state='sent', sent_ts=?, error='' WHERE id=?", (ts, item_id))

    def retry(self, item_id: int, at: float, error: str) -> None:
        self.db.execute("UPDATE outbox SET state='pending', retry_at=?, error=? WHERE id=?", (at, error, item_id))

    def fail(self, item_id: int, state: str, error: str) -> None:
        """Mark a post failed (permanent) or ambiguous (maybe delivered; never resent automatically).

        Posts that depend on it, directly or transitively, become ``blocked`` so they
        are visible to the operator instead of silently pending forever.
        """
        with self.db.transaction():
            self.db.execute("UPDATE outbox SET state=?, error=? WHERE id=?", (state, error, item_id))
            self.db.execute(DEPENDENTS + " UPDATE outbox SET state='blocked', error='waiting for a post that failed'"
                            " WHERE state='pending' AND idem_key IN (SELECT key FROM dependents) AND id!=?",
                            (item_id, item_id))

    def requeue(self, item_id: int) -> bool:
        """Operator retry of a failed or ambiguous post; its blocked dependents wait on it again."""
        item = self.by_id(item_id)
        if item is None or item.state not in ("failed", "ambiguous"):
            return False
        with self.db.transaction():
            self.db.execute("UPDATE outbox SET state='pending', retry_at=0, attempts=0, error='' WHERE id=?", (item_id,))
            self.db.execute(DEPENDENTS + " UPDATE outbox SET state='pending', error='' WHERE state='blocked'"
                            " AND idem_key IN (SELECT key FROM dependents)", (item_id,))
        return True

    def list(self, *, states: tuple[str, ...] = ("failed", "ambiguous", "blocked"), limit: int = 100) -> list[OutboxItem]:
        rows = self.db.all(f"SELECT * FROM outbox WHERE state IN ({','.join('?' * len(states))}) ORDER BY id DESC LIMIT ?",
                           (*states, limit))
        return [codec.outbox(row) for row in rows]

    def for_session(self, session_id: str) -> list[OutboxItem]:
        return [codec.outbox(row) for row in self.db.all("SELECT * FROM outbox WHERE session_id=? ORDER BY id",
                                                         (session_id,))]
