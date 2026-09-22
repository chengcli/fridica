from __future__ import annotations

from dataclasses import asdict, replace
import fcntl
import json
import os
from pathlib import Path
import sqlite3
import time

from .models import AgentResult, Message


class Store:
    def __init__(self, path: Path, *, control: bool = False):
        path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        self._lock = None
        if not control:
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
            CREATE TABLE IF NOT EXISTS runtime (
                id INTEGER PRIMARY KEY CHECK(id=1), pid INTEGER NOT NULL,
                started_at REAL NOT NULL, heartbeat_at REAL NOT NULL,
                status TEXT NOT NULL, observe_only INTEGER NOT NULL
            );
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
            CREATE TABLE IF NOT EXISTS file_requests (
                id TEXT PRIMARY KEY,
                event_id TEXT NOT NULL UNIQUE REFERENCES events(event_id),
                sender TEXT NOT NULL,
                channel TEXT NOT NULL,
                operation TEXT NOT NULL,
                path TEXT NOT NULL,
                content TEXT NOT NULL,
                before_content TEXT,
                status TEXT NOT NULL DEFAULT 'pending',
                created REAL NOT NULL,
                error TEXT,
                result TEXT,
                notified INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS grants (
                id TEXT PRIMARY KEY,
                sender TEXT NOT NULL,
                channel TEXT NOT NULL,
                path TEXT NOT NULL,
                expires REAL,
                revoked INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS cooldowns (
                workspace TEXT NOT NULL,
                channel TEXT NOT NULL,
                last_response REAL NOT NULL,
                PRIMARY KEY(workspace,channel)
            );
        """)
        columns = {row[1] for row in self.connection.execute("PRAGMA table_info(events)")}
        if "reply_only" not in columns:
            self.connection.execute("ALTER TABLE events ADD COLUMN reply_only INTEGER NOT NULL DEFAULT 0")
        columns = {row[1] for row in self.connection.execute("PRAGMA table_info(tasks)")}
        if "session" not in columns:
            self.connection.execute("ALTER TABLE tasks ADD COLUMN session TEXT")
        for name, definition in {"control_state": "TEXT NOT NULL DEFAULT 'active'", "pause_reason": "TEXT",
                                 "reset_at": "REAL NOT NULL DEFAULT 0", "control_revision": "INTEGER NOT NULL DEFAULT 0",
                                 "root_thread": "TEXT", "continuation": "TEXT", "digest_pending": "INTEGER NOT NULL DEFAULT 0", "debriefed_turn": "INTEGER NOT NULL DEFAULT 0"}.items():
            if name not in columns:
                self.connection.execute(f"ALTER TABLE tasks ADD COLUMN {name} {definition}")
        self.connection.execute("CREATE TABLE IF NOT EXISTS thread_decisions (workspace TEXT, channel TEXT, thread TEXT, action TEXT, decided_at REAL)")
        from .collaboration import initialize
        initialize(self.connection)
        self.connection.commit()
        if not control:
            with self.connection:
                self.connection.execute("UPDATE tasks SET digest_pending=0")
                self.connection.execute("UPDATE tasks SET continuation='failed' WHERE continuation='pending'")
                self.connection.execute("UPDATE events SET state='interrupted' WHERE state='running'")
                self.connection.execute("UPDATE events SET state='ambiguous' WHERE state='sending'")
                self.connection.execute("UPDATE file_requests SET status='interrupted' WHERE status='applying'")

    def heartbeat(self, status: str, observe_only: bool, started_at: float) -> None:
        with self.connection:
            self.connection.execute(
                "INSERT OR REPLACE INTO runtime VALUES(1,?,?,?,?,?)",
                (os.getpid(), started_at, time.time(), status, int(observe_only)),
            )

    @staticmethod
    def _key(message: Message) -> tuple[str, str, str]:
        """The (workspace, channel, thread) tuple that identifies a task row."""
        return message.workspace_id, message.channel_id, message.thread_id

    def close(self) -> None:
        self.connection.close()
        if self._lock is not None:
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
            if not self.connection.in_transaction:
                self.connection.execute('BEGIN IMMEDIATE')
            task = self.task(message)
            payload = asdict(message)
            decision = None
            if task and task['control_state'] != 'active':
                state, decision = 'observed', task['control_state']
                if decision == 'cleaned':
                    payload['text'] = ''
            cursor = self.connection.execute(
                "INSERT OR IGNORE INTO events(event_id,payload,workspace,channel,thread,timestamp,state,decision) VALUES(?,?,?,?,?,?,?,?)",
                (message.event_id, json.dumps(payload), message.workspace_id, message.channel_id,
                 message.thread_id, float(message.timestamp), state, decision),
            )
        return cursor.rowcount == 1

    def pending(self) -> list[sqlite3.Row]:
        return self.connection.execute(
            "SELECT e.* FROM events e WHERE e.state IN ('pending','ready') AND e.retry_at<=? "
            "AND (e.state='ready' OR EXISTS (SELECT 1 FROM tasks t WHERE t.workspace=e.workspace AND t.channel=e.channel AND t.thread=e.thread AND t.control_state!='active') OR (NOT EXISTS ("
            "SELECT 1 FROM file_requests r JOIN events original ON r.event_id=original.event_id "
            "WHERE r.notified=0 AND original.workspace=e.workspace AND original.channel=e.channel AND original.thread=e.thread) "
            "AND NOT EXISTS (SELECT 1 FROM events earlier WHERE earlier.state='ready' "
            "AND earlier.workspace=e.workspace AND earlier.channel=e.channel AND earlier.thread=e.thread))) "
            "ORDER BY e.timestamp LIMIT 100",
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
            (*self._key(message), float(message.timestamp), message.event_id, limit),
        ).fetchall()
        if message.thread_id == message.timestamp:
            rows = self.connection.execute(
                "SELECT payload FROM events WHERE workspace=? AND channel=? AND thread=CAST(json_extract(payload,'$.timestamp') AS TEXT) "
                "AND timestamp<=? AND event_id!=? ORDER BY timestamp DESC LIMIT ?",
                (message.workspace_id, message.channel_id, float(message.timestamp), message.event_id, limit),
            ).fetchall()
        return [Message(**json.loads(row["payload"])) for row in reversed(rows)]

    def thread_messages(self, message: Message, limit: int) -> list[Message]:
        """Every stored message of the thread, oldest first, including the latest delivered reply."""
        rows = self.connection.execute(
            "SELECT payload FROM events WHERE workspace=? AND channel=? AND thread=? "
            "AND json_extract(payload,'$.text')!='' ORDER BY timestamp DESC LIMIT ?",
            (*self._key(message), limit),
        ).fetchall()
        return [Message(**json.loads(row["payload"])) for row in reversed(rows)]

    def waiting_threads(self, workspace: str) -> list[Message]:
        """The latest message of every active thread whose last reply asked for information."""
        rows = self.connection.execute(
            "SELECT e.payload FROM tasks t JOIN events e ON e.event_id=("
            "SELECT x.event_id FROM events x WHERE x.workspace=t.workspace AND x.channel=t.channel AND x.thread=t.thread "
            "ORDER BY x.timestamp DESC LIMIT 1) WHERE t.workspace=? AND t.status='waiting' AND t.control_state='active'",
            (workspace,)).fetchall()
        return [Message(**json.loads(row["payload"])) for row in rows]

    def clear_text(self, message: Message) -> None:
        """Drop the text of an event that arrived for a thread whose contents were cleared."""
        with self.connection:
            self.connection.execute("UPDATE events SET payload=? WHERE event_id=?",
                                    (json.dumps(asdict(replace(message, text=""))), message.event_id))

    def pause_for_turn_limit(self, message: Message) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE tasks SET control_state='paused',pause_reason='Thread turn limit reached.',"
                "control_revision=control_revision+1 WHERE workspace=? AND channel=? AND thread=? AND control_state='active'",
                self._key(message))

    def mark_debriefed_at_limit(self, message: Message) -> None:
        """A thread debriefed at its turn limit needs no continuation summary."""
        with self.connection:
            self.connection.execute("UPDATE tasks SET continuation='debriefed' WHERE workspace=? AND channel=? AND thread=? "
                                    "AND continuation IS NULL", self._key(message))

    def has_open_file_requests(self, message: Message) -> bool:
        """True while a scoped file operation in this thread still awaits the owner's decision or execution."""
        return self.connection.execute(
            "SELECT 1 FROM file_requests r JOIN events e ON r.event_id=e.event_id "
            "WHERE e.workspace=? AND e.channel=? AND e.thread=? AND r.status IN ('pending','approved','applying') LIMIT 1",
            self._key(message),
        ).fetchone() is not None

    def claim_debrief(self, message: Message) -> bool:
        """Claim the debrief for the thread's current turn; False when this turn was already debriefed."""
        with self.connection:
            cursor = self.connection.execute(
                "UPDATE tasks SET debriefed_turn=turns,digest_pending=1 WHERE workspace=? AND channel=? AND thread=? AND debriefed_turn<turns",
                self._key(message),
            )
        return cursor.rowcount == 1

    def record_debrief(self, message: Message) -> None:
        """Add the posted debrief to the activity feed."""
        with self.connection:
            self.connection.execute("INSERT INTO thread_decisions VALUES(?,?,?,?,?)",
                                    (*self._key(message), "debriefed", time.time()))

    def begin_continuation(self, message: Message) -> bool:
        """Claim the one-time wrap-up of a thread; False when it already happened or is under way."""
        with self.connection:
            cursor = self.connection.execute(
                "UPDATE tasks SET continuation='pending' WHERE workspace=? AND channel=? AND thread=? AND continuation IS NULL",
                self._key(message),
            )
        return cursor.rowcount == 1

    def finish_continuation(self, message: Message, continuation: str, task_id: str | None, session: str | None) -> None:
        """Record where a wrapped-up thread continues and seed that new thread with a fresh budget.

        ``continuation`` is the new thread's root timestamp, or ``failed`` when no summary
        could be posted. The exhausted thread stays paused so the owner can still resume it.
        """
        with self.connection:
            reason = ("Thread reached its turn limit; the discussion continues in a new thread."
                      if continuation != "failed" else "Thread reached its turn limit; the summary post failed.")
            self.connection.execute(
                "UPDATE tasks SET continuation=?, pause_reason=?, "
                "control_state=CASE WHEN control_state='active' THEN 'paused' ELSE control_state END, "
                "control_revision=control_revision+1 WHERE workspace=? AND channel=? AND thread=?",
                (continuation, reason, *self._key(message)),
            )
            if task_id is not None and continuation != "failed":
                self.connection.execute(
                    "INSERT INTO tasks(workspace,channel,thread,task_id,status,turns,updated,session) VALUES(?,?,?,?,?,?,?,?) "
                    "ON CONFLICT(workspace,channel,thread) DO NOTHING",
                    (message.workspace_id, message.channel_id, continuation, task_id, "complete", 0, time.time(), session),
                )
                self.connection.execute("UPDATE tasks SET root_thread=? WHERE workspace=? AND channel=? AND thread=?",
                                        (self.task(message)["root_thread"] or message.thread_id, message.workspace_id, message.channel_id, continuation))
                self.connection.execute("INSERT INTO thread_decisions VALUES(?,?,?,?,?)",
                                        (*self._key(message), "continued", time.time()))

    def task(self, message: Message) -> sqlite3.Row | None:
        return self.connection.execute(
            "SELECT * FROM tasks WHERE workspace=? AND channel=? AND thread=?",
            self._key(message),
        ).fetchone()

    def pause_loop(self, message: Message, max_turns: int, max_wait_replies: int) -> None:
        task = self.task(message)
        if task is None or task['control_state'] != 'active':
            return
        reason = None
        if task['turns'] >= max_turns:
            reason = f'Thread reached the {max_turns}-turn limit.'
        else:
            rows = self.connection.execute(
                "SELECT json_extract(result,'$.status') AS status FROM events e "
                "WHERE workspace=? AND channel=? AND thread=? AND state='sent' AND reply_only=0 "
                "AND result IS NOT NULL AND timestamp>? "
                "AND NOT (json_extract(result,'$.status')='waiting' AND EXISTS ("
                "SELECT 1 FROM file_requests r WHERE r.event_id=e.event_id)) "
                "ORDER BY CAST(sent_ts AS REAL) DESC LIMIT ?",
                (*self._key(message), task['reset_at'], max_wait_replies),
            ).fetchall()
            if len(rows) == max_wait_replies and all(r['status'] == 'waiting' for r in rows):
                reason = f'{max_wait_replies} consecutive replies needed more information; possible conversation loop.'
        if reason:
            with self.connection:
                self.connection.execute("UPDATE tasks SET control_state='paused',pause_reason=?,control_revision=control_revision+1 "
                                        "WHERE workspace=? AND channel=? AND thread=? AND control_state='active'",
                                        (reason, *self._key(message)))

    def awaiting_delivery(self, message: Message) -> bool:
        return self.connection.execute(
            "SELECT 1 FROM events WHERE workspace=? AND channel=? AND thread=? AND state='ready' LIMIT 1",
            self._key(message),
        ).fetchone() is not None

    def begin(self, message: Message, task_id: str, turn: int) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state='running',decision='respond',task_id=?,turn=? WHERE event_id=?",
                (task_id, turn, message.event_id),
            )
            self.connection.execute(
                "INSERT INTO tasks(workspace,channel,thread,task_id,status,turns,updated) VALUES(?,?,?,?,?,?,?) "
                "ON CONFLICT(workspace,channel,thread) "
                "DO UPDATE SET task_id=excluded.task_id,status=excluded.status,turns=excluded.turns,updated=excluded.updated",
                (*self._key(message), task_id, "running", turn, time.time()),
            )

    def save_session(self, message: Message, session: str | None) -> None:
        """Record the backend session that continues this thread's conversation."""
        with self.connection:
            self.connection.execute(
                "UPDATE tasks SET session=? WHERE workspace=? AND channel=? AND thread=?",
                (session, *self._key(message)),
            )

    def save_result(self, message: Message, result: AgentResult) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE file_requests SET notified=1 WHERE event_id=? "
                "AND status IN ('complete','failed','declined','interrupted')", (message.event_id,)
            )
            self.connection.execute(
                "UPDATE events SET state='ready',result=? WHERE event_id=?", (json.dumps(asdict(result)), message.event_id)
            )
            self.connection.execute(
                "UPDATE tasks SET status=?,updated=? WHERE workspace=? AND channel=? AND thread=?",
                ("delivery_pending", time.time(), *self._key(message)),
            )

    def save_notice(self, message: Message, result: AgentResult, task_id: str, turn: int) -> None:
        with self.connection:
            self.connection.execute(
                "UPDATE events SET state='ready',decision='respond',result=?,task_id=?,turn=?,reply_only=1 WHERE event_id=?",
                (json.dumps(asdict(result)), task_id, turn, message.event_id),
            )

    def delivered(self, message: Message, timestamp: str) -> None:
        with self.connection:
            self.connection.execute("UPDATE events SET state='sent',sent_ts=? WHERE event_id=?", (timestamp, message.event_id))
            if self.get(message.event_id)["reply_only"]:
                return
            result = json.loads(self.get(message.event_id)["result"])
            self.connection.execute(
                "UPDATE tasks SET status=?,updated=? WHERE workspace=? AND channel=? AND thread=?",
                (result["status"], time.time(), *self._key(message)),
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
