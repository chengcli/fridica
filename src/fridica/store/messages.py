"""Inbound messages and the per-thread inbox."""

from __future__ import annotations

import json

from ..core.models import InboxItem, Message, ThreadKey
from . import codec
from .db import Database


class Messages:
    def __init__(self, db: Database):
        self.db = db

    def intake(self, message: Message, now: float, *, work: bool = True) -> tuple[str, int | None]:
        """Persist a Slack message, create its thread, and queue it for the thread's actor.

        One transaction, so a crash can never leave a message without its inbox row.
        Returns ``(session_id, inbox_id)``; ``inbox_id`` is None for duplicates, for
        our own posts (``source='self'``), and with ``work=False`` (history only).
        """
        key = message.key
        with self.db.transaction():
            cursor = self.db.execute(
                "INSERT OR IGNORE INTO messages (event_id, workspace, channel, ts, root_ts, thread_ts, sender, text,"
                " files_json, source, meta_json, received_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?)",
                (message.event_id, message.workspace, message.channel, message.ts, key.root_ts, message.thread_ts,
                 message.sender, message.text, codec.dumps(list(message.files)), message.source,
                 codec.meta_json(message.meta), now),
            )
            if cursor.rowcount == 0:
                return key.id, None
            ensure_thread(self.db, key, now)
            if message.source == "self" or not work:
                return key.id, None
            inbox = self.db.execute(
                "INSERT INTO thread_inbox (session_id, kind, ref, created) VALUES (?, 'message', ?, ?)",
                (key.id, message.event_id, now),
            )
            return key.id, inbox.lastrowid

    def get(self, event_id: str) -> Message | None:
        row = self.db.one("SELECT * FROM messages WHERE event_id=?", (event_id,))
        return codec.message(row) if row else None

    def latest_ts(self, workspace: str, channel: str, *, received_before: float | None = None) -> float | None:
        """The Slack timestamp of the newest message stored in a channel (before ``received_before``), or None."""
        sql = "SELECT MAX(CAST(ts AS REAL)) FROM messages WHERE workspace=? AND channel=?"
        parameters: tuple = (workspace, channel)
        if received_before is not None:
            sql += " AND received_at < ?"
            parameters += (received_before,)
        row = self.db.one(sql, parameters)
        return row[0] if row and row[0] is not None else None

    def exists(self, workspace: str, channel: str, ts: str) -> bool:
        return self.db.one("SELECT 1 FROM messages WHERE workspace=? AND channel=? AND ts=?",
                           (workspace, channel, ts)) is not None

    def verdict(self, event_id: str) -> str:
        row = self.db.one("SELECT verdict FROM messages WHERE event_id=?", (event_id,))
        return row[0] if row else ""

    def set_verdict(self, event_id: str, verdict: str) -> None:
        self.db.execute("UPDATE messages SET verdict=? WHERE event_id=?", (verdict, event_id))

    def thread(self, key: ThreadKey, *, limit: int = 50, before_ts: str | None = None) -> list[Message]:
        """The newest ``limit`` messages of a thread (including our own posts), oldest first."""
        sql = "SELECT * FROM messages WHERE workspace=? AND channel=? AND root_ts=?"
        parameters: list = [key.workspace, key.channel, key.root_ts]
        if before_ts is not None:
            sql += " AND CAST(ts AS REAL) <= CAST(? AS REAL)"
            parameters.append(before_ts)
        rows = self.db.all(sql + " ORDER BY CAST(ts AS REAL) DESC LIMIT ?", (*parameters, limit))
        return [codec.message(row) for row in reversed(rows)]

    def channel_recent(self, workspace: str, channel: str, before_ts: str, *, limit: int = 20) -> list[Message]:
        """Recent top-level channel messages before ``before_ts``: the social context of a new thread."""
        rows = self.db.all(
            "SELECT * FROM messages WHERE workspace=? AND channel=? AND ts=root_ts"
            " AND CAST(ts AS REAL) < CAST(? AS REAL) ORDER BY CAST(ts AS REAL) DESC LIMIT ?",
            (workspace, channel, before_ts, limit),
        )
        return [codec.message(row) for row in reversed(rows)]

    def wipe(self, key: ThreadKey) -> int:
        """Erase the text of every message in a thread (the dashboard's clean action)."""
        return self.db.execute("UPDATE messages SET text='', files_json='[]' WHERE workspace=? AND channel=? AND root_ts=?",
                               (key.workspace, key.channel, key.root_ts)).rowcount

    def latest_unanswered(self, key: ThreadKey) -> Message | None:
        """The newest human message that came after our last post in the thread."""
        row = self.db.one(
            "SELECT * FROM messages WHERE workspace=? AND channel=? AND root_ts=? AND source!='self'"
            " AND meta_json IS NULL AND CAST(ts AS REAL) > COALESCE((SELECT MAX(CAST(ts AS REAL)) FROM messages"
            " WHERE workspace=? AND channel=? AND root_ts=? AND source='self'), 0)"
            " ORDER BY CAST(ts AS REAL) DESC LIMIT 1",
            (key.workspace, key.channel, key.root_ts) * 2,
        )
        return codec.message(row) if row else None


def ensure_thread(db: Database, key: ThreadKey, now: float) -> None:
    db.execute(
        "INSERT OR IGNORE INTO threads (id, workspace, channel, root_ts, created, updated) VALUES (?,?,?,?,?,?)",
        (key.id, key.workspace, key.channel, key.root_ts, now, now),
    )


class Inbox:
    def __init__(self, db: Database):
        self.db = db

    def add(self, session_id: str, kind: str, now: float, *, ref: str = "", payload: dict | None = None) -> int:
        cursor = self.db.execute(
            "INSERT INTO thread_inbox (session_id, kind, ref, payload_json, created) VALUES (?,?,?,?,?)",
            (session_id, kind, ref, codec.dumps(payload or {}), now),
        )
        return cursor.lastrowid

    def add_once(self, session_id: str, kind: str, ref: str, now: float, payload: dict) -> int:
        with self.db.transaction():
            row = self.db.one("SELECT id FROM thread_inbox WHERE session_id=? AND kind=? AND ref=?",
                              (session_id, kind, ref))
            return row[0] if row else self.add(session_id, kind, now, ref=ref, payload=payload)

    def instructions(self, session_id: str, *, limit: int = 20) -> list[dict]:
        rows = self.db.all("SELECT id, state, created, payload_json FROM thread_inbox"
                           " WHERE session_id=? AND kind='owner_instruction' ORDER BY id DESC LIMIT ?",
                           (session_id, limit))
        return [{"id": row[0], "state": row[1], "created": row[2], "text": json.loads(row[3])["text"]}
                for row in rows]

    def wipe_instructions(self, session_id: str) -> None:
        self.db.execute("UPDATE thread_inbox SET payload_json=? WHERE session_id=? AND kind='owner_instruction'",
                        (codec.dumps({"text": ""}), session_id))

    def claim(self, session_id: str) -> InboxItem | None:
        """Mark the thread's oldest pending item as processing and return it."""
        with self.db.transaction():
            row = self.db.one("SELECT * FROM thread_inbox WHERE session_id=? AND state='pending' ORDER BY id LIMIT 1",
                              (session_id,))
            if row is None:
                return None
            self.db.execute("UPDATE thread_inbox SET state='processing' WHERE id=?", (row["id"],))
        return codec.inbox(row)

    def finish(self, inbox_id: int, state: str = "done") -> None:
        self.db.execute("UPDATE thread_inbox SET state=? WHERE id=?", (state, inbox_id))

    def retry_or_drop(self, inbox_id: int, limit: int) -> str:
        """After a failure: back to pending for another attempt, or dropped once ``limit`` attempts failed.

        Returns the new state, or "" when the item was already committed (it is left alone).
        """
        with self.db.transaction():
            row = self.db.one("SELECT attempts FROM thread_inbox WHERE id=? AND state='processing'", (inbox_id,))
            if row is None:
                return ""
            state = "pending" if row[0] + 1 < limit else "dropped"
            self.db.execute("UPDATE thread_inbox SET attempts=attempts+1, state=? WHERE id=?", (state, inbox_id))
            return state

    def release_session(self, session_id: str) -> int:
        """Return a thread's in-flight items to pending (its actor died)."""
        return self.db.execute("UPDATE thread_inbox SET state='pending' WHERE session_id=? AND state='processing'",
                               (session_id,)).rowcount

    def get(self, inbox_id: int) -> InboxItem | None:
        row = self.db.one("SELECT * FROM thread_inbox WHERE id=?", (inbox_id,))
        return codec.inbox(row) if row else None

    def pending_sessions(self) -> list[str]:
        return [row[0] for row in self.db.all(
            "SELECT DISTINCT session_id FROM thread_inbox WHERE state='pending' ORDER BY session_id")]

    def pending(self, session_id: str) -> list[InboxItem]:
        return [codec.inbox(row) for row in self.db.all(
            "SELECT * FROM thread_inbox WHERE session_id=? AND state='pending' ORDER BY id", (session_id,))]

    def payload(self, inbox_id: int) -> dict:
        row = self.db.one("SELECT payload_json FROM thread_inbox WHERE id=?", (inbox_id,))
        return json.loads(row[0]) if row else {}
