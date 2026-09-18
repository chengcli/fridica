from __future__ import annotations

from dataclasses import asdict
import fcntl
import json
import os
from pathlib import Path
import sqlite3
import time

from .models import AgentResult, Message


class Store:
    def __init__(self, path: Path):
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self._lock = path.with_suffix(".lock").open("a")
        os.chmod(path.with_suffix(".lock"), 0o600)
        try:
            fcntl.flock(self._lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            self._lock.close()
            raise ValueError("another fridica process is using this state database") from None
        self.connection = sqlite3.connect(path)
        os.chmod(path, 0o600)
        self.connection.row_factory = sqlite3.Row
        self.connection.executescript("""
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS events (
                event_id TEXT PRIMARY KEY,
                payload TEXT NOT NULL,
                workspace TEXT NOT NULL,
                channel TEXT NOT NULL,
                thread TEXT NOT NULL,
                timestamp REAL NOT NULL,
                state TEXT NOT NULL DEFAULT 'pending',
                decision TEXT,
                result TEXT,
                task_id TEXT,
                turn INTEGER NOT NULL DEFAULT 0,
                sent_ts TEXT,
                retry_at REAL NOT NULL DEFAULT 0,
                attempts INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS events_context ON events(workspace,channel,thread,timestamp);
            CREATE UNIQUE INDEX IF NOT EXISTS events_message ON events(workspace,channel,json_extract(payload,'$.timestamp'));
            CREATE TABLE IF NOT EXISTS identity (owner TEXT NOT NULL, workspace TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tasks (
                workspace TEXT NOT NULL,
                channel TEXT NOT NULL,
                thread TEXT NOT NULL,
                task_id TEXT NOT NULL,
                status TEXT NOT NULL,
                turns INTEGER NOT NULL DEFAULT 0,
                updated REAL NOT NULL,
                PRIMARY KEY(workspace,channel,thread)
            );
            CREATE TABLE IF NOT EXISTS cooldowns (
                workspace TEXT NOT NULL,
                channel TEXT NOT NULL,
                last_response REAL NOT NULL,
                PRIMARY KEY(workspace,channel)
            );
        """)
        with self.connection:
            self.connection.execute("UPDATE events SET state='interrupted' WHERE state='running'")
            self.connection.execute("UPDATE events SET state='ambiguous' WHERE state='sending'")

    def close(self) -> None:
        self.connection.close()
        self._lock.close()

    def bind(self, owner: str, workspace: str) -> None:
        row = self.connection.execute("SELECT owner,workspace FROM identity").fetchone()
        if row is not None and tuple(row) != (owner, workspace):
            raise ValueError("state database belongs to another Slack identity; choose a separate state_path")
        if row is None:
            with self.connection:
                self.connection.execute("INSERT INTO identity VALUES(?,?)", (owner, workspace))

    def add(self, message: Message, state: str = "pending") -> bool:
        with self.connection:
            cursor = self.connection.execute(
                "INSERT OR IGNORE INTO events(event_id,payload,workspace,channel,thread,timestamp,state) VALUES(?,?,?,?,?,?,?)",
                (message.event_id, json.dumps(asdict(message)), message.workspace_id, message.channel_id,
                 message.thread_id, float(message.timestamp), state),
            )
        return cursor.rowcount == 1

    def pending(self) -> list[sqlite3.Row]:
        return self.connection.execute(
            "SELECT * FROM events WHERE state IN ('pending','ready') AND retry_at<=? ORDER BY timestamp LIMIT 100",
            (time.time(),),
        ).fetchall()

    def get(self, event_id: str) -> sqlite3.Row:
        return self.connection.execute("SELECT * FROM events WHERE event_id=?", (event_id,)).fetchone()

    def mark(self, event_id: str, state: str, decision: str | None = None) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state=?, decision=COALESCE(?,decision) WHERE event_id=?", (state, decision, event_id)
            )

    def context(self, message: Message, limit: int) -> list[Message]:
        rows = self.connection.execute(
            "SELECT payload FROM events WHERE workspace=? AND channel=? AND thread=? "
            "AND timestamp<=? AND event_id!=? ORDER BY timestamp DESC LIMIT ?",
            (message.workspace_id, message.channel_id, message.thread_id, float(message.timestamp), message.event_id, limit),
        ).fetchall()
        if message.thread_id == message.timestamp:
            rows = self.connection.execute(
                "SELECT payload FROM events WHERE workspace=? AND channel=? AND thread=CAST(json_extract(payload,'$.timestamp') AS TEXT) "
                "AND timestamp<=? AND event_id!=? ORDER BY timestamp DESC LIMIT ?",
                (message.workspace_id, message.channel_id, float(message.timestamp), message.event_id, limit),
            ).fetchall()
        return [Message(**json.loads(row["payload"])) for row in reversed(rows)]

    def task(self, message: Message) -> sqlite3.Row | None:
        return self.connection.execute(
            "SELECT * FROM tasks WHERE workspace=? AND channel=? AND thread=?",
            (message.workspace_id, message.channel_id, message.thread_id),
        ).fetchone()

    def awaiting_delivery(self, message: Message) -> bool:
        return self.connection.execute(
            "SELECT 1 FROM events WHERE workspace=? AND channel=? AND thread=? AND state='ready' LIMIT 1",
            (message.workspace_id, message.channel_id, message.thread_id),
        ).fetchone() is not None

    def begin(self, message: Message, task_id: str, turn: int) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state='running',decision='respond',task_id=?,turn=? WHERE event_id=?",
                (task_id, turn, message.event_id),
            )
            self.connection.execute(
                "INSERT INTO tasks VALUES(?,?,?,?,?,?,?) ON CONFLICT(workspace,channel,thread) "
                "DO UPDATE SET task_id=excluded.task_id,status=excluded.status,turns=excluded.turns,updated=excluded.updated",
                (message.workspace_id, message.channel_id, message.thread_id, task_id, "running", turn, time.time()),
            )

    def save_result(self, message: Message, result: AgentResult) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state='ready',result=? WHERE event_id=?", (json.dumps(asdict(result)), message.event_id)
            )
            self.connection.execute(
                "UPDATE tasks SET status=?,updated=? WHERE workspace=? AND channel=? AND thread=?",
                ("delivery_pending", time.time(), message.workspace_id, message.channel_id, message.thread_id),
            )

    def delivered(self, message: Message, timestamp: str) -> None:
        with self.connection:
            self.connection.execute("UPDATE events SET state='sent',sent_ts=? WHERE event_id=?", (timestamp, message.event_id))
            result = json.loads(self.get(message.event_id)["result"])
            self.connection.execute(
                "UPDATE tasks SET status=?,updated=? WHERE workspace=? AND channel=? AND thread=?",
                (result["status"], time.time(), message.workspace_id, message.channel_id, message.thread_id),
            )
            self.connection.execute(
                "INSERT INTO cooldowns VALUES(?,?,?) ON CONFLICT(workspace,channel) DO UPDATE SET last_response=excluded.last_response",
                (message.workspace_id, message.channel_id, time.time()),
            )

    def retry(self, event_id: str, delay: float) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state='ready',retry_at=?,attempts=attempts+1 WHERE event_id=?", (time.time()+delay, event_id)
            )

    def cooling_down(self, message: Message, duration: float) -> bool:
        row = self.connection.execute(
            "SELECT last_response FROM cooldowns WHERE workspace=? AND channel=?", (message.workspace_id, message.channel_id)
        ).fetchone()
        return row is not None and time.time() - row[0] < duration

    def attention(self) -> list[sqlite3.Row]:
        return self.connection.execute(
            "SELECT event_id,state FROM events WHERE state IN ('interrupted','ambiguous','failed','blocked') "
            "OR json_extract(result,'$.status')='blocked'"
        ).fetchall()
