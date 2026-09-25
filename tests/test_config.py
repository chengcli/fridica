from importlib.resources import files
from pathlib import Path, PurePosixPath

import pytest

from fridica.config import load_config
from fridica.core.errors import ConfigError

from helpers import base_config


def test_loads_machines_workspaces_and_defaults(config, workspace):
    assert config.owner.slack_user == "UOWNER"
    assert config.slack.channels == ("CROOM", "COTHER")
    assert config.slack.may_delegate("CROOM")
    assert config.machines.names == ("local", "snowy", "dart9")
    assert config.parent.default_machine == "local"
    local = config.machines["local"]
    assert local.workspace("project").path == workspace.resolve()
    assert local.default_backend == "claude"
    snowy = config.machines["snowy"]
    assert snowy.host == "snowy" and snowy.remote and snowy.tags == ("cuda", "rtx5090")
    assert snowy.workspace("exocubed").path == PurePosixPath("~/scix/repos/exocubed")
    assert snowy.default_backend == "codex" and snowy.max_jobs == 2
    assert snowy.resources.environment() == {"OMP_NUM_THREADS": "32", "CUDA_VISIBLE_DEVICES": "0"}
    assert config.machines["dart9"].backends == ("codex",)
    assert config.limits.max_wait_replies == 3 and config.policy.mode == "write"
    socket = config.state.control_socket  # beside the state database, unless that path is too long for a socket
    assert len(str(socket)) <= 100 and (socket.name == "control.sock" or socket.name.startswith("control-"))
    assert len(config.fingerprint) == 64


def test_machine_and_workspace_policy_overrides(write_config):
    config = load_config(write_config(machines="""
        [machines.box]
        host = "box"
        policy = { network = ["*"], approvals = "never" }
        [machines.box.workspaces.careful]
        path = "/careful"
        policy = { approvals = "auto" }
        [machines.box.workspaces]
        data = { path = "/data", policy = { mode = "read-only" } }
        work = "/work"
    """))
    box = config.machines["box"]
    assert box.transport == "ssh"
    assert box.policy.any_network and box.policy.approvals == "never"
    assert box.workspace("data").policy.mode == "read-only" and not box.workspace("data").writable
    assert box.workspace("work").policy.mode == "write"
    assert box.workspace("work").policy.network == ("*",)
    assert box.workspace("careful").policy.approvals == "auto"


def test_payload_hides_paths(config):
    payload = config.machines.payload({"snowy": 1})
    snowy = next(item for item in payload if item["name"] == "snowy")
    assert snowy["workspaces"] == {"exocubed": "write", "canoe": "write"}
    assert snowy["busy_jobs"] == 1
    assert "/home" not in str(payload) and "scix" not in str(payload)


@pytest.mark.parametrize("machines, message", [
    ("[machines.x]\ntransport = 'ssh'\n[machines.x.workspaces]\nw = '/w'\n", "host must be"),
    ("[machines.x]\nhost = 'x'\n", "at least one entry"),
    ("[machines.x]\nhost = 'x'\n[machines.x.workspaces]\nw = 'relative'\n", "remote paths"),
    ("[machines.x]\nhost = 'x'\nbackends = ['gpt']\n[machines.x.workspaces]\nw = '/w'\n", "backends"),
    ("[machines.x]\nhost = 'x'\nbackends = ['codex']\ndefault_backend = 'claude'\n[machines.x.workspaces]\nw = '/w'\n",
     "default_backend"),
    ("[machines.x]\nhost = 'x'\nmax_workers = 1\nmax_jobs = 2\n[machines.x.workspaces]\nw = '/w'\n", "max_jobs"),
    ("[machines.x]\nhost = 'x'\npolicy = { mode = 'yolo' }\n[machines.x.workspaces]\nw = '/w'\n", "policy.mode"),
    ("[machines.x]\nhost = 'x'\npolicy = { gpu_confine = true }\n[machines.x.workspaces]\nw = '/w'\n", "gpu_confine"),
    ("[machines.x]\nhost = 'x'\nslurm = { partition = 'gpu' }\n[machines.x.workspaces]\nw = '/w'\n", "slurm options"),
    ("[machines.x]\nhost = 'x'\ncolor = 'red'\n[machines.x.workspaces]\nw = '/w'\n", "unknown keys"),
    ("[machines.X]\nhost = 'x'\n[machines.X.workspaces]\nw = '/w'\n", "machine name"),
    ("[machines.x]\nhost = 'x'\n[machines.x.workspaces]\nw = '/'\n", "too broad"),
    ("[machines.x]\ntransport = 'local'\nhost = 'x'\n[machines.x.workspaces]\nw = '/tmp'\n", "takes no host"),
    ("[machines.x]\nhost = 'x'\n[machines.x.workspaces]\nw = '/work/../..'\n", "may not contain"),
    ("[machines.x]\nhost = 'x'\n[machines.x.workspaces]\nw = '//'\n", "too broad"),
    ("[machines.x]\nhost = 'x'\n[machines.x.workspaces]\nw = { path = '/w', policy = { gpu_confine = true } }\n", "needs resources.gpus"),
    ("[machines.x]\nhost = 'x'\nresources = { gpus = [0] }\npolicy = { gpu_confine = true }\n[machines.x.workspaces]\n"
     "w = { path = '/w', policy = { mode = 'read-only' } }\n", "cannot enforce read-only"),
])
def test_rejects_invalid_machines(write_config, machines, message):
    with pytest.raises(ConfigError, match=message):
        load_config(write_config(machines=machines))


def test_rejects_unknown_top_level_and_missing_sections(write_config, workspace, tmp_path):
    text = base_config(workspace, tmp_path / "s.sqlite3")
    with pytest.raises(ConfigError, match="unknown keys"):
        load_config(write_config(text + "\n[extra]\nx = 1\n"))
    with pytest.raises(ConfigError, match=r"missing \[machines\]"):
        load_config(write_config(text.split("[machines.local]")[0]))


def test_rejects_bad_identity_and_channels(write_config, workspace, tmp_path):
    text = base_config(workspace, tmp_path / "s.sqlite3")
    with pytest.raises(ConfigError, match="slack_user"):
        load_config(write_config(text.replace('"UOWNER"', '"owner"')))
    with pytest.raises(ConfigError, match="channel IDs"):
        load_config(write_config(text.replace('["CROOM", "COTHER"]', '["#general"]')))
    with pytest.raises(ConfigError, match="subset"):
        load_config(write_config(text.replace('channels = ["CROOM", "COTHER"]',
                                              'channels = ["CROOM"]\ndelegate_channels = ["COTHER"]')))


def test_local_workspace_must_exist_and_state_must_be_outside(write_config, workspace, tmp_path):
    with pytest.raises(ConfigError, match="not an existing directory"):
        load_config(write_config(base_config(tmp_path / "missing", tmp_path / "s.sqlite3")))
    with pytest.raises(ConfigError, match="state.path must be outside"):
        load_config(write_config(base_config(workspace, workspace / "state.sqlite3")))


def test_writable_workspace_may_not_contain_the_config_or_fridica(tmp_path):
    config_dir = tmp_path / "etc"
    config_dir.mkdir()
    path = config_dir / "config.toml"
    path.write_text(base_config(tmp_path, tmp_path.parent / "elsewhere.sqlite3"))
    with pytest.raises(ConfigError, match="inside writable workspace"):
        load_config(path)
    path.write_text(base_config(tmp_path, tmp_path.parent / "elsewhere.sqlite3").replace(
        f'project = "{tmp_path}"', f'project = {{ path = "{tmp_path}", policy = {{ mode = "read-only" }} }}'))
    assert load_config(path).machines["local"].workspace("project").policy.mode == "read-only"


def test_limits_and_parent_validation(write_config, workspace, tmp_path):
    text = base_config(workspace, tmp_path / "s.sqlite3")
    config = load_config(write_config(text + "\n[limits]\nmax_wait_replies = 9\nauto_resume = true\njob_timeout = 60\n"
                                      "\n[parent]\nbackend = 'codex'\nreasoning_effort = 'high'\n"))
    assert config.limits.max_wait_replies == 9 and config.limits.auto_resume and config.limits.job_timeout == 60.0
    assert config.parent.backend == "codex" and config.parent.reasoning_effort == "high"
    with pytest.raises(ConfigError, match="limits.max_wait_replies"):
        load_config(write_config(text + "\n[limits]\nmax_wait_replies = 0\n"))
    with pytest.raises(ConfigError, match="unknown keys"):
        load_config(write_config(text + "\n[limits]\nmax_turns = 6\n"))
    with pytest.raises(ConfigError, match="limits.auto_resume"):
        load_config(write_config(text + "\n[limits]\nauto_resume = 1\n"))
    with pytest.raises(ConfigError, match="default machine"):
        load_config(write_config(text + "\n[parent]\ndefault_machine = 'nope'\n"))


def test_contract_beside_config_is_picked_up(write_config):
    path = write_config()
    (path.parent / "contract.md").write_text("## Participation\n\n## Replies\n")
    assert load_config(path).owner.contract == (path.parent / "contract.md").resolve()


def test_packaged_template_loads_after_filling_placeholders(tmp_path):
    project = tmp_path / "project"
    project.mkdir()
    text = files("fridica.config").joinpath("template.toml").read_text()
    text = text.replace('"~/project"', f'"{project}"').replace(
        '"~/.local/state/fridica/state.sqlite3"', f'"{tmp_path / "state.sqlite3"}"')
    text = text.replace("U000OWNER", "U0OWNER").replace("T000TEAM", "T0TEAM").replace("C000CHANNEL", "C0ROOM")
    (tmp_path / "etc").mkdir()
    path = tmp_path / "etc" / "config.toml"
    path.write_text(text)
    assert load_config(path).machines.names == ("local",)
    head, machines = text.split("# [machines.snowy]", 1)
    machines, tail = ("# [machines.snowy]" + machines).split("[state]", 1)
    machines = "\n".join(line[2:] if line.startswith("# ") else line for line in machines.splitlines())
    path.write_text(head + machines + "\n[state]" + tail)
    assert load_config(path).machines.names == ("local", "snowy", "dart9", "greatlakes")


def test_tokens_are_read_from_the_environment(config, monkeypatch):
    monkeypatch.setenv("SLACK_APP_TOKEN", "xapp-1")
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxb-wrong")
    with pytest.raises(ConfigError, match="user token"):
        config.tokens()
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxp-1")
    assert config.tokens() == ("xapp-1", "xoxp-1")


def test_missing_file_mentions_init(tmp_path):
    with pytest.raises(ConfigError, match="fridica init"):
        load_config(Path(tmp_path / "none.toml"))


def test_long_state_paths_get_a_short_control_socket(write_config, workspace, tmp_path, monkeypatch):
    monkeypatch.delenv("XDG_RUNTIME_DIR", raising=False)
    deep = tmp_path / ("d" * 60) / ("e" * 60) / "state.sqlite3"
    config = load_config(write_config(base_config(workspace, deep)))
    assert len(str(config.state.control_socket)) < 100 and config.state.control_socket.name.startswith("control-")
    with pytest.raises(ConfigError, match="too long"):
        load_config(write_config(base_config(workspace, deep) + f'control_socket = "{deep.parent}/c.sock"\n'))


def test_gpu_confine_turns_on_for_machines_that_declare_gpus(write_config):
    config = load_config(write_config(machines="""
        [machines.gpu]
        host = "gpu"
        resources = { gpus = [0, 1] }
        [machines.gpu.workspaces]
        work = "/work"
        notes = { path = "/notes", policy = { mode = "read-only" } }
        raw = { path = "/raw", policy = { mode = "full" } }
        plain = { path = "/plain", policy = { gpu_confine = false } }

        [machines.cpu]
        host = "cpu"
        [machines.cpu.workspaces]
        work = "/work"
    """))
    gpu = config.machines["gpu"]
    assert [(item.name, item.policy.gpu_confine) for item in gpu.workspaces] == [
        ("work", True), ("notes", False), ("raw", False), ("plain", False)]
    assert config.machines["cpu"].workspace("work").policy.gpu_confine is False
    assert config.machines["local"].workspace("project").policy.gpu_confine is False
    assert gpu.policy.gpu_confine is False  # the machine-level default stays automatic, resolved per workspace


def test_global_opt_out_of_gpu_confine(write_config, workspace, tmp_path):
    text = base_config(workspace, tmp_path / "s.sqlite3", """
        [machines.gpu]
        host = "gpu"
        resources = { gpus = [0] }
        [machines.gpu.workspaces]
        work = "/work"
    """) + "\n[policy]\ngpu_confine = false\n"
    assert load_config(write_config(text)).machines["gpu"].workspace("work").policy.gpu_confine is False


def test_machine_concurrency_defaults(write_config):
    config = load_config(write_config(machines="""
        [machines.box]
        host = "box"
        [machines.box.workspaces]
        work = "/work"
    """))
    box = config.machines["box"]
    assert (box.max_workers, box.max_jobs) == (4, 2)


def test_workspace_subfolders_option(write_config):
    config = load_config(write_config(machines="""
        [machines.gpu]
        host = "gpu"
        [machines.gpu.workspaces]
        shared = "/data/ai"
        repo = { path = "/data/repo", subfolders = false }
        docs = { path = "/data/docs", policy = { mode = "read-only" } }
    """))
    gpu = config.machines["gpu"]
    assert gpu.workspace("shared").subfolders  # on by default for writable workspaces
    assert not gpu.workspace("repo").subfolders and not gpu.workspace("docs").subfolders
    with pytest.raises(ConfigError, match="subfolders need a writable"):
        load_config(write_config(machines="""
            [machines.gpu]
            host = "gpu"
            [machines.gpu.workspaces]
            shared = { path = "/data/ai", subfolders = true, policy = { mode = "read-only" } }
        """))
