from pathlib import Path
import re
import sqlite3

import pytest

import fridica
from fridica.store import Store
from fridica.store.schema import MIGRATIONS, migrate, version


def test_migrations_are_idempotent_and_versioned(tmp_path):
    connection = sqlite3.connect(tmp_path / "db.sqlite3", isolation_level=None)
    assert version(connection) == 0
    assert migrate(connection) == len(MIGRATIONS)
    assert migrate(connection) == len(MIGRATIONS)
    assert version(connection) == len(MIGRATIONS)


def test_newer_schema_is_refused(tmp_path):
    connection = sqlite3.connect(tmp_path / "db.sqlite3", isolation_level=None)
    migrate(connection)
    connection.execute("UPDATE meta SET value='999' WHERE key='schema_version'")
    with pytest.raises(RuntimeError, match="newer"):
        migrate(connection)


def test_no_ddl_outside_the_schema_module():
    root = Path(fridica.__file__).parent
    offenders = []
    for path in root.rglob("*.py"):
        if "legacy" in path.parts or path.name == "schema.py":
            continue
        if re.search(r"\b(CREATE|ALTER|DROP)\s+(TABLE|INDEX)\b", path.read_text()):
            offenders.append(str(path.relative_to(root)))
    assert offenders == []


def test_single_daemon_lock_and_identity_binding(tmp_path):
    path = tmp_path / "state.sqlite3"
    first = Store(path)
    with pytest.raises(RuntimeError, match="another fridica daemon"):
        Store(path)
    first.db.bind("UOWNER", "TTEAM")
    first.db.bind("UOWNER", "TTEAM")
    with pytest.raises(RuntimeError, match="another Slack identity"):
        first.db.bind("USOMEONE", "TTEAM")
    first.close()
    second = Store(path)
    second.close()
    assert oct(path.stat().st_mode & 0o777) == "0o600"


def test_nested_transactions_roll_back_together(store):
    with pytest.raises(ZeroDivisionError):
        with store.transaction():
            store.audit.record("me", "outer", 1.0)
            with store.transaction():
                store.audit.record("me", "inner", 1.0)
            1 / 0
    assert store.audit.recent() == []
