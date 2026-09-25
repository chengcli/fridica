import os
import sys

import pytest

from fridica.config import load_config
from fridica.core.models import Message
from fridica.store import Store

from helpers import FAKE_SSH, ROOM, TEAM, base_config


@pytest.fixture
def workspace(tmp_path):
    path = tmp_path / "project"
    path.mkdir()
    return path


@pytest.fixture
def write_config(tmp_path, workspace):
    def write(text: str | None = None, *, machines: str = "", name: str = "config.toml"):
        path = tmp_path / "etc" / name
        path.parent.mkdir(exist_ok=True)
        path.write_text(text if text is not None else base_config(workspace, tmp_path / "state" / "db.sqlite3", machines))
        return path
    return write


@pytest.fixture
def config(write_config):
    return load_config(write_config(machines="""
        [machines.snowy]
        transport = "ssh"
        host = "snowy"
        tags = ["cuda", "rtx5090"]
        backends = ["codex", "claude"]
        max_workers = 3
        max_jobs = 2
        resources = { cpus = 32, gpus = [0], gpu_type = "RTX 5090" }

        [machines.snowy.workspaces]
        exocubed = "~/scix/repos/exocubed"
        canoe = "/home/me/canoe"

        [machines.dart9]
        transport = "ssh"
        host = "me@dart9"
        tags = ["cuda", "gcc"]
        backends = ["codex"]

        [machines.dart9.workspaces]
        canoe = "/home/me/canoe"
    """))


@pytest.fixture
def store(tmp_path):
    database = Store(tmp_path / "state" / "store.sqlite3")
    yield database
    database.close()


@pytest.fixture
def message():
    counter = iter(range(1, 10_000))

    def make(text="<@UOWNER> help", *, ts=None, thread_ts=None, sender="UALICE", channel=ROOM, meta=None,
             source="socket", event_id=None):
        number = next(counter)
        ts = ts or f"100.{number:06d}"
        return Message(event_id or f"event{number}", TEAM, channel, ts, thread_ts, sender, text, source=source,
                       meta=meta)
    return make



@pytest.fixture
def fake_ssh(tmp_path, monkeypatch):
    """Put a fake ssh on PATH that runs the remote script locally; returns its JSONL log path."""
    executable = tmp_path / "bin" / "ssh"
    executable.parent.mkdir(exist_ok=True)
    executable.write_text(FAKE_SSH.format(python=sys.executable))
    executable.chmod(0o700)
    log = tmp_path / "ssh.log"
    monkeypatch.setenv("SSH_LOG", str(log))
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    monkeypatch.setenv("PATH", str(executable.parent) + os.pathsep + os.environ["PATH"])
    return log


@pytest.fixture
def fake_agents(tmp_path, monkeypatch):
    """Fake codex, claude, and bwrap executables on PATH; returns a reader for their JSONL log."""
    import json
    from fakes import FAKE_APP_SERVER, FAKE_BWRAP, FAKE_CLAUDE
    log = tmp_path / "worker.log"
    monkeypatch.setenv("WORKER_LOG", str(log))
    monkeypatch.setenv("BWRAP_LOG", str(tmp_path / "bwrap.log"))
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxp-secret")
    binaries = tmp_path / "agents"
    binaries.mkdir(exist_ok=True)
    for name, body in (("codex", FAKE_APP_SERVER), ("claude", FAKE_CLAUDE), ("bwrap", FAKE_BWRAP)):
        executable = binaries / name
        executable.write_text(body.replace("{python}", sys.executable))
        executable.chmod(0o700)
    monkeypatch.setenv("PATH", str(binaries) + os.pathsep + os.environ["PATH"])

    def entries(kind=None):
        rows = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        return [row for row in rows if kind is None or row["kind"] == kind]
    return entries
