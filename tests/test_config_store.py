from dataclasses import replace
from pathlib import Path

import pytest

from fridica.config import load_config
from fridica.models import AgentResult
from fridica.store import Store


@pytest.mark.parametrize("changes", [
    {"channels": ()}, {"channels": "CROOM"}, {"owner_id": "wrong"},
    {"timeout": float("nan")}, {"cooldown": -1}, {"max_turns": True},
    {"context_limit": 1.5}, {"backend": "other"}, {"general_messages": "yes"}, {"resume_sessions": 1}, {"contract": Path("relative.md")}, {"session_timeout": -1}, {"session_timeout": "2w"}, {"allowed_domains": ["not a host"]},
    {"allowed_domains": ["github.com/path"]}, {"allowed_domains": "github.com"}, {"allowed_domains": ["localhost"]},
    {"allowed_domains": ["**"]}, {"allowed_domains": ["*github.com"]},
    {"app_token_env": "invalid-name"},
])
def test_invalid_config(config, changes):
    with pytest.raises(ValueError):
        replace(config, **changes)


def test_paths_and_tokens(config, monkeypatch):
    with pytest.raises(ValueError):
        replace(config, state_path=config.workspace / "state.sqlite3")
    with pytest.raises(ValueError):
        replace(config, workspace=config.workspace / "missing")
    monkeypatch.delenv(config.app_token_env, raising=False)
    with pytest.raises(ValueError):
        config.tokens()
    monkeypatch.setenv(config.app_token_env, "xapp-test")
    monkeypatch.setenv(config.user_token_env, "xoxp-test")
    assert config.tokens() == ("xapp-test", "xoxp-test")


def test_load_config(config, tmp_path):
    source = tmp_path / "config.toml"
    source.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{config.workspace}"\nstate_path="{config.state_path}"\n')
    loaded = load_config(source)
    assert loaded.owner_id == config.owner_id
    assert tuple(loaded.channels) == config.channels
    assert loaded.workspace == config.workspace
    with source.open("a") as stream:
        stream.write('unknown = true\n')
    with pytest.raises(ValueError, match="unknown"):
        load_config(source)


def test_dedup_context_and_database_lock(store, config, message):
    first = message()
    assert store.add(first)
    assert not store.add(first)
    assert not store.add(message("same_slack_message"))
    with pytest.raises(ValueError, match="another"):
        Store(config.state_path)
    other = message("other", timestamp="101.000001", thread_id="101.000001")
    store.add(other)
    reply = message("reply", timestamp="102.000001")
    store.add(reply)
    assert store.context(reply, 50) == [first]
    assert store.context(reply, 1) == [first]


def test_store_identity_binding(store):
    store.bind("UOWNER", "TTEAM")
    store.bind("UOWNER", "TTEAM")
    with pytest.raises(ValueError, match="identity"):
        store.bind("UOTHER", "TTEAM")


def test_restart_preserves_work_and_does_not_repeat_uncertain_actions(config, message):
    database = Store(config.state_path)
    running = message()
    sending = message("sending", timestamp="101.000001", thread_id="101.000001")
    ready = message("ready", timestamp="102.000001", thread_id="102.000001")
    for entry in (running, sending, ready):
        database.add(entry)
        database.begin(entry, entry.event_id, 3)
    database.save_result(sending, AgentResult("done"))
    database.mark(sending.event_id, "sending")
    database.save_result(ready, AgentResult("ready"))
    database.delivered(ready, "103.000001")
    database.retry(ready.event_id, 0)
    database.close()
    database = Store(config.state_path)
    try:
        assert database.get(running.event_id)["state"] == "interrupted"
        assert database.get(sending.event_id)["state"] == "ambiguous"
        assert [entry["event_id"] for entry in database.pending()] == ["ready"]
        assert database.task(running)["turns"] == 3
        assert database.cooling_down(ready, 60)
    finally:
        database.close()


def test_tasks_table_gains_session_column(config):
    import sqlite3
    connection = sqlite3.connect(config.state_path)
    connection.executescript(
        "CREATE TABLE tasks (workspace TEXT NOT NULL, channel TEXT NOT NULL, thread TEXT NOT NULL, task_id TEXT NOT NULL,"
        " status TEXT NOT NULL, turns INTEGER NOT NULL DEFAULT 0, updated REAL NOT NULL, PRIMARY KEY(workspace,channel,thread));"
        "INSERT INTO tasks VALUES('TTEAM','CROOM','100.000001','task','waiting',1,0);"
    )
    connection.close()
    database = Store(config.state_path)
    try:
        row = database.connection.execute("SELECT * FROM tasks").fetchone()
        assert row["session"] is None and row["task_id"] == "task"
    finally:
        database.close()


def test_allowed_domains_accepts_wildcards(config, tmp_path):
    assert replace(config, allowed_domains=("*",)).allowed_domains == ("*",)
    source = tmp_path / "config.toml"
    source.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{config.workspace}"\n'
                      f'state_path="{config.state_path}"\nallowed_domains=["GitHub.com", "*.PyPI.org", "github.com", "*"]\n')
    assert load_config(source).allowed_domains == ("github.com", "*.pypi.org", "*")
