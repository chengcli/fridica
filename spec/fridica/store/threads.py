"""Thread sessions and per-channel cooldowns."""

from __future__ import annotations

from dataclasses import asdict, replace

from ..core.models import ThreadKey, ThreadSession
from . import codec
from .db import Database
from .messages import ensure_thread


class StaleSession(RuntimeError):
    """The session changed since it was loaded (for example by a dashboard action)."""


class Threads:
    def __init__(self, db: Database):
        self.db = db

    def get(self, session_id: str) -> ThreadSession | None:
        row = self.db.one("SELECT * FROM threads WHERE id=?", (session_id,))
        return codec.session(row) if row else None

    def ensure(self, key: ThreadKey, now: float) -> ThreadSession:
        with self.db.transaction():
            ensure_thread(self.db, key, now)
        return self.get(key.id)

    def save(self, session: ThreadSession, now: float) -> ThreadSession:
        """Write the whole session if nobody else changed it since it was read."""
        cursor = self.db.execute(
            "UPDATE threads SET status=?, control=?, pause_reason=?, turns=?, wait_streak=?, no_progress=?,"
            " last_reply_hash=?, reset_at=?, summary=?, decisions_json=?, context_json=?,"
            " debriefed_turn=?, last_unsolicited=?, updated=?, version=version+1"
            " WHERE id=? AND version=?",
            (session.status, session.control, session.pause_reason, session.turns, session.wait_streak,
             session.no_progress, session.last_reply_hash, session.reset_at, session.summary,
             codec.dumps(list(session.decisions)), codec.dumps(asdict(session.context)),
             session.debriefed_turn, session.last_unsolicited, now, session.id,
             session.version),
        )
        if cursor.rowcount != 1:
            raise StaleSession(session.id)
        return replace(session, updated=now, version=session.version + 1)

    def list(self, *, control: tuple[str, ...] | None = None, limit: int = 100, channel: str | None = None) -> list[ThreadSession]:
        sql, parameters = "SELECT * FROM threads WHERE 1=1", []
        if control:
            sql += f" AND control IN ({','.join('?' * len(control))})"
            parameters.extend(control)
        if channel:
            sql += " AND channel=?"
            parameters.append(channel)
        rows = self.db.all(sql + " ORDER BY updated DESC LIMIT ?", (*parameters, limit))
        return [codec.session(row) for row in rows]

    def needing_attention(self) -> list[ThreadSession]:
        rows = self.db.all("SELECT * FROM threads WHERE control='paused' OR"
                           " (control='active' AND status='blocked') ORDER BY updated DESC")
        return [codec.session(row) for row in rows]

    def with_status(self, status: str) -> list[ThreadSession]:
        return [codec.session(row) for row in self.db.all(
            "SELECT * FROM threads WHERE status=? AND control='active'", (status,))]


class Cooldowns:
    def __init__(self, db: Database):
        self.db = db

    def cooling(self, workspace: str, channel: str, now: float, seconds: float) -> bool:
        row = self.db.one("SELECT last_unsolicited FROM cooldowns WHERE workspace=? AND channel=?", (workspace, channel))
        return row is not None and now - row[0] < seconds

    def mark(self, workspace: str, channel: str, now: float) -> None:
        self.db.execute("INSERT OR REPLACE INTO cooldowns VALUES (?,?,?)", (workspace, channel, now))
