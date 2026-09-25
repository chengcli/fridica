"""Task notes (revisioned per thread) and the audit log."""

from __future__ import annotations

import json

from . import codec
from .db import Database


class Notes:
    def __init__(self, db: Database):
        self.db = db

    def current(self, session_id: str) -> tuple[int, dict]:
        row = self.db.one("SELECT revision, data_json FROM notes WHERE session_id=? ORDER BY revision DESC LIMIT 1",
                          (session_id,))
        return (row[0], json.loads(row[1])) if row else (0, {})

    def write(self, session_id: str, data: dict, actor: str, now: float, *, source: str = "",
              expected: int | None = None) -> int:
        """Store a new revision; with ``expected``, refuse if someone else wrote first."""
        revision, _ = self.current(session_id)
        if expected is not None and expected != revision:
            raise ValueError(f"notes changed (revision {revision}, expected {expected})")
        self.db.execute("INSERT INTO notes (session_id, revision, actor, data_json, source, created) VALUES (?,?,?,?,?,?)",
                        (session_id, revision + 1, actor, codec.dumps(data), source, now))
        return revision + 1

    def history(self, session_id: str) -> list[dict]:
        return [{"revision": row[0], "actor": row[1], "data": json.loads(row[2]), "source": row[3], "created": row[4]}
                for row in self.db.all("SELECT revision, actor, data_json, source, created FROM notes"
                                       " WHERE session_id=? ORDER BY revision", (session_id,))]


class Audit:
    def __init__(self, db: Database):
        self.db = db

    def record(self, actor: str, action: str, now: float, *, target: str = "", details: dict | None = None) -> None:
        self.db.execute("INSERT INTO audit (time, actor, action, target, details_json) VALUES (?,?,?,?,?)",
                        (now, actor, action, target, codec.dumps(details or {})))

    def recent(self, *, limit: int = 200, target: str | None = None) -> list[dict]:
        sql, parameters = "SELECT time, actor, action, target, details_json FROM audit", ()
        if target is not None:
            sql, parameters = sql + " WHERE target=?", (target,)
        return [{"time": row[0], "actor": row[1], "action": row[2], "target": row[3], "details": json.loads(row[4])}
                for row in self.db.all(sql + " ORDER BY id DESC LIMIT ?", (*parameters, limit))]


class ParentTurns:
    """One row per parent LLM call, written together with the effects of the inbox item it served."""

    def __init__(self, db: Database):
        self.db = db

    def add(self, session_id: str, inbox_id: int, call: dict, now: float, *, action: dict | None = None) -> None:
        self.db.execute(
            "INSERT INTO parent_turns (session_id, inbox_id, backend, model, call, action_json, prompt_chars, latency_ms,"
            " error, created) VALUES (?,?,?,?,?,?,?,?,?,?)",
            (session_id, inbox_id, call.get("backend", ""), call.get("model", ""), call.get("call", ""),
             codec.dumps(action) if action is not None else None, call.get("prompt_chars", 0),
             call.get("latency_ms", 0), call.get("error", ""), now))

    def for_session(self, session_id: str, *, limit: int = 50) -> list[dict]:
        return [dict(row) for row in self.db.all(
            "SELECT * FROM parent_turns WHERE session_id=? ORDER BY id DESC LIMIT ?", (session_id, limit))]
