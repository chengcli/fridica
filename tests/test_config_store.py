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


# ----- remote working folders -----

def write_remote_config(tmp_path, workspace, **extra):
    lines = ['owner_id="UOWNER"', 'workspace_id="TTEAM"', 'channels=["CROOM"]', f'workspace="{workspace}"',
             f'state_path="{tmp_path / "state.sqlite3"}"']
    for key, value in extra.items():
        rendered = value if isinstance(value, str) and value.startswith(("[", "true", "false")) else f'"{value}"'
        lines.append(f"{key}={rendered}")
    source = tmp_path / "remote.toml"
    source.write_text("\n".join(lines) + "\n")
    return source


def test_parse_root_forms(tmp_path):
    from pathlib import PurePosixPath
    from fridica.config import parse_root
    assert parse_root("dart9:/mnt/xxx") == ("dart9", PurePosixPath("/mnt/xxx"))
    assert parse_root("cheng@dart9.example.edu:/data/run 1") == ("cheng@dart9.example.edu", PurePosixPath("/data/run 1"))
    host, path = parse_root(str(tmp_path))
    assert host is None and path == tmp_path
    host, path = parse_root("~/projects/x")
    assert host is None and path.is_absolute()
    host, path = parse_root("/odd:name/dir")
    assert host is None and path == Path("/odd:name/dir")
    for bad in ("dart9:relative/path", "-oProxyCommand=evil:/x", "bad host:/x", "dart9:", ""):
        with pytest.raises(ValueError):
            parse_root(bad)


def test_remote_workspace_config(tmp_path):
    from pathlib import PurePosixPath
    source = write_remote_config(tmp_path, "dart9:/mnt/xxx", additional_workspaces='["dart9:/mnt/data"]')
    config = load_config(source)
    assert config.remote and config.ssh_host == "dart9"
    assert config.workspace == PurePosixPath("/mnt/xxx") and not (tmp_path / "mnt").exists()
    assert config.additional_workspaces == (PurePosixPath("/mnt/data"),)
    assert config.root_label(config.workspace) == "dart9:/mnt/xxx"
    explicit = load_config(write_remote_config(tmp_path, "/mnt/xxx", ssh_host="dart9"))
    assert explicit.ssh_host == "dart9" and explicit.workspace == PurePosixPath("/mnt/xxx")
    local = tmp_path / "local"
    local.mkdir()
    assert replace(config, ssh_host=None, workspace=local, additional_workspaces=()).root_label(local) == str(local)


@pytest.mark.parametrize("workspace,extra,match", [
    ("dart9:/mnt/xxx", {"additional_workspaces": '["/local/data"]'}, "prefix every other root"),
    ("dart9:/mnt/xxx", {"read_only_workspaces": '["snowy:/data"]'}, "same host"),
    ("dart9:/mnt/xxx", {"ssh_host": "snowy"}, "disagrees"),
    ("dart9:/mnt/xxx", {"file_access": "true"}, "file_access requires local"),
    ("dart9:/", {}, "absolute POSIX"),
    ("dart9:relative", {}, "absolute path"),
    ("-evil:/x", {}, "invalid SSH host"),
    ("/mnt/xxx", {"ssh_host": "bad host"}, "ssh_host must be"),
])
def test_remote_workspace_rejections(tmp_path, workspace, extra, match):
    with pytest.raises(ValueError, match=match):
        load_config(write_remote_config(tmp_path, workspace, **extra))


def test_multi_host_roots_and_resources(tmp_path, config):
    from pathlib import PurePosixPath
    from fridica.config import Host, Resources
    source = tmp_path / "multi.toml"
    source.write_text(f'''owner_id="UOWNER"
workspace_id="TTEAM"
channels=["CROOM"]
workspace="{config.workspace}"
additional_workspaces=["dart9:/mnt/data1/projects", "snowy:/scratch/a", "dart9:/mnt/data1/shared"]
state_path="{config.state_path}"
heavy_tasks=true
[resources.local]
cpus = 2
[resources.dart9]
cpus = 6
gpus = [0, 1]
''')
    loaded = load_config(source)
    assert not loaded.remote and loaded.ssh_host is None and loaded.additional_workspaces == ()
    assert loaded.resources == Resources(cpus=2)
    assert loaded.remote_hosts == (
        Host("dart9", (PurePosixPath("/mnt/data1/projects"), PurePosixPath("/mnt/data1/shared")), Resources(cpus=6, gpus=(0, 1))),
        Host("snowy", (PurePosixPath("/scratch/a"),)),
    )
    assert [host.name for host in loaded.hosts] == ["local", "dart9", "snowy"]
    assert loaded.primary.workspace == config.workspace and not loaded.primary.remote
    assert loaded.host("dart9").workspace == PurePosixPath("/mnt/data1/projects") and loaded.host("dart9").ssh_host == "dart9"
    assert loaded.host("dart9").resources.gpu_worker and not loaded.host("snowy").resources.gpu_worker
    assert loaded.host("dart9").payload() == {"name": "dart9", "roots": ["/mnt/data1/projects", "/mnt/data1/shared"],
                                             "resources": {"cpus": 6, "gpus": [0, 1], "gpu_access": True}}
    assert loaded.root_labels() == [str(config.workspace), "dart9:/mnt/data1/projects", "dart9:/mnt/data1/shared", "snowy:/scratch/a"]
    with pytest.raises(KeyError):
        loaded.host("nowhere")
    # A plain [resources] table describes the workspace's host; other hosts get defaults.
    source.write_text(source.read_text().replace("[resources.local]\ncpus = 2\n[resources.dart9]\ncpus = 6\ngpus = [0, 1]\n", "[resources]\ncpus = 3\n"))
    flat = load_config(source)
    assert flat.resources == Resources(cpus=3) and flat.host("dart9").resources == Resources()
    # Hot reload compares configurations by value.
    assert load_config(source) == flat


@pytest.mark.parametrize("body,match", [
    ("[resources.snowy]\ncpus = 2\n", "names a host that has no workspace roots"),
    ("[resources.dart9]\ntpus = 2\n", "accepts cpus"),
    ("[resources]\ncpus = 2\n[resources.dart9]\ncpus = 4\n", "not both"),
    ("[resources.dart9]\ncpus = 0\n", "positive integer"),
    ('additional_workspaces=[]\nfile_access=true\nheavy_tasks=true\n', "needs a remote host"),
    ('resources = 3\n', "resources must be"),
])
def test_multi_host_rejections(tmp_path, config, body, match):
    source = tmp_path / "multi.toml"
    extra = "" if "additional_workspaces" in body else 'additional_workspaces=["dart9:/mnt/data1/projects"]\n'
    source.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{config.workspace}"\n'
                      f'state_path="{config.state_path}"\n{extra}{body}')
    with pytest.raises(ValueError, match=match):
        load_config(source)


def test_remote_workspace_with_further_hosts(tmp_path):
    from pathlib import PurePosixPath
    source = write_remote_config(tmp_path, "dart9:/mnt/xxx", additional_workspaces='["dart9:/mnt/data", "snowy:/scratch"]')
    loaded = load_config(source)
    assert loaded.ssh_host == "dart9" and loaded.additional_workspaces == (PurePosixPath("/mnt/data"),)
    assert [host.name for host in loaded.remote_hosts] == ["snowy"] and loaded.primary.roots == (PurePosixPath("/mnt/xxx"), PurePosixPath("/mnt/data"))
    with pytest.raises(ValueError, match="same host as workspace"):
        load_config(write_remote_config(tmp_path, "dart9:/mnt/xxx", read_only_workspaces='["snowy:/data"]', file_access="true"))


def test_file_access_and_network_default_on_for_local_roots(tmp_path):
    (tmp_path / "work").mkdir()
    loaded = load_config(write_remote_config(tmp_path, tmp_path / "work"))
    assert loaded.file_access is True and loaded.allowed_domains == ("*",)


def test_file_access_defaults_follow_the_workspace_host(tmp_path):
    from pathlib import PurePosixPath
    (tmp_path / "work").mkdir()
    assert load_config(write_remote_config(tmp_path, "dart9:/mnt/xxx")).file_access is False
    # Remote roots only host heavy tasks; the local roots stay under scoped file access.
    remote_extra = write_remote_config(tmp_path, tmp_path / "work", additional_workspaces='["dart9:/mnt/a"]', heavy_tasks="true")
    loaded = load_config(remote_extra)
    assert loaded.file_access is True and [host.name for host in loaded.heavy_hosts] == ["dart9"]
    assert loaded.heavy_hosts[0].roots == (PurePosixPath("/mnt/a"),) and [host.name for host in loaded.hosts] == ["local", "dart9"]
    # Heavy tasks with nowhere else to run fall back to the agent's native tools.
    heavy = load_config(write_remote_config(tmp_path, tmp_path / "work", heavy_tasks="true"))
    assert heavy.file_access is False and [host.name for host in heavy.heavy_hosts] == ["local"]
    explicit = write_remote_config(tmp_path, tmp_path / "work", heavy_tasks="true", file_access="true")
    with pytest.raises(ValueError, match="needs a remote host"):
        load_config(explicit)
    # Explicit file_access next to a remote heavy-task host is fine.
    assert load_config(write_remote_config(tmp_path, tmp_path / "work", additional_workspaces='["dart9:/mnt/a"]',
                                           file_access="true")).file_access is True
