"""The SQLite connection: single-daemon lock, migrations, identity, and transactions.

Only the daemon writes. The CLI and dashboard reach state through the daemon's
control API; ``Database(path, lock=False, readonly=True)`` exists for tools that
must inspect a stopped daemon's state.
"""

from __future__ import annotations

from collections.abc import Iterator
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import sqlite3

from .schema import migrate


class Database:
    def __init__(self, path: Path, *, lock: bool = True, readonly: bool = False):
        self.path = path
        self._lock = None
        self._depth = 0
        if readonly:
            self.connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True, isolation_level=None)
        else:
            path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
            if lock:
                lock_path = path.with_suffix(".lock")
                self._lock = lock_path.open("a")
                os.chmod(lock_path, 0o600)
                try:
                    fcntl.flock(self._lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    self._lock.close()
                    raise RuntimeError("another fridica daemon is using this state database") from None
            self.connection = sqlite3.connect(path, isolation_level=None)
            os.chmod(path, 0o600)
            self.connection.execute("PRAGMA journal_mode=WAL")
            self.connection.execute("PRAGMA foreign_keys=ON")
            migrate(self.connection)
        self.connection.row_factory = sqlite3.Row

    @contextmanager
    def transaction(self) -> Iterator[sqlite3.Connection]:
        """One atomic unit of work; nested calls join the outer transaction.

        Transactions must never span an ``await``: the daemon is a single asyncio
        thread, so a transaction held across a suspension would be joined by
        unrelated work.
        """
        if self._depth:
            self._depth += 1
            try:
                yield self.connection
            finally:
                self._depth -= 1
            return
        self.connection.execute("BEGIN IMMEDIATE")
        self._depth = 1
        try:
            yield self.connection
        except BaseException:
            self.connection.execute("ROLLBACK")
            raise
        else:
            self.connection.execute("COMMIT")
        finally:
            self._depth = 0

    def execute(self, sql: str, parameters: tuple | dict = ()) -> sqlite3.Cursor:
        return self.connection.execute(sql, parameters)

    def one(self, sql: str, parameters: tuple | dict = ()) -> sqlite3.Row | None:
        return self.connection.execute(sql, parameters).fetchone()

    def all(self, sql: str, parameters: tuple | dict = ()) -> list[sqlite3.Row]:
        return self.connection.execute(sql, parameters).fetchall()

    def meta(self, key: str) -> str | None:
        row = self.one("SELECT value FROM meta WHERE key=?", (key,))
        return row[0] if row else None

    def bind(self, owner: str, workspace: str) -> None:
        """Tie this database to one Slack identity; refuse to reuse it for another."""
        with self.transaction():
            stored = (self.meta("owner"), self.meta("workspace"))
            if stored == (None, None):
                self.execute("INSERT INTO meta VALUES ('owner', ?), ('workspace', ?)", (owner, workspace))
            elif stored != (owner, workspace):
                raise RuntimeError("state database belongs to another Slack identity; choose a separate [state] path")

    def close(self) -> None:
        self.connection.close()
        if self._lock is not None:
            self._lock.close()
            self._lock = None
