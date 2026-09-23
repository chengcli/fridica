"""Remote working folders: the ssh wrapper, the per-turn round trip through it, and the checks."""
import asyncio
from dataclasses import replace
import json
import os
from pathlib import PurePosixPath
import re
import subprocess
import sys

import pytest

from fridica import agents, checks, remote
from fridica.agents import ClaudeBackend, CodexBackend
from fridica.models import ConversationContext, Decision
from fridica.runner import BackendError, run as _run

FAKE_SSH = f'''#!{sys.executable}
"""A stand-in for ssh: checks the client's options and runs the remote script locally with sh -c."""
import json, os, pathlib, subprocess, sys
arguments = sys.argv[1:]
assert arguments[0] == "-T", arguments
assert "BatchMode=yes" in arguments and "--" in arguments, arguments
host, script = arguments[arguments.index("--") + 1], arguments[arguments.index("--") + 2]
assert len(arguments) == arguments.index("--") + 3, arguments
assert host == os.environ["EXPECTED_HOST"], host
pathlib.Path(os.environ["SSH_LOG"]).open("a").write(json.dumps({{"host": host, "script": script}}) + "\\n")
if os.environ.get("SSH_FAIL"):
    print("ssh: connect to host " + host + " port 22: Connection refused", file=sys.stderr)
    sys.exit(255)
# ssh hands the command line to the remote login shell; "exec sh -c '...'" is what it receives.
sys.exit(subprocess.run(["bash", "-c", script]).returncode)
'''


@pytest.fixture
def remote_config(config, tmp_path):
    """A configuration whose workspace is 'dart9:<tmp_path>/remote', served by the fake ssh."""
    workspace = tmp_path / "remote"
    workspace.mkdir()
    return replace(config, ssh_host="dart9", workspace=PurePosixPath(workspace))


@pytest.fixture
def fake_ssh(tmp_path, monkeypatch):
    executable = tmp_path / "bin" / "ssh"
    executable.parent.mkdir()
    executable.write_text(FAKE_SSH)
    executable.chmod(0o700)
    log = tmp_path / "ssh.log"
    monkeypatch.setenv("SSH_LOG", str(log))
    monkeypatch.setenv("EXPECTED_HOST", "dart9")
    monkeypatch.setenv("PATH", str(executable.parent) + os.pathsep + os.environ["PATH"])
    return log


def test_ssh_command_shape(remote_config, tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path))
    argv = remote.ssh_command(remote_config, "exec sh -c 'true'")
    assert argv[0] == "ssh" and argv[1] == "-T" and argv[-3:] == ["--", "dart9", "exec sh -c 'true'"]
    assert "BatchMode=yes" in argv and "ConnectTimeout=15" in argv and "ControlMaster=auto" in argv
    control = tmp_path / "fridica"
    assert f"ControlPath={control}/%C" in argv and control.is_dir() and (control.stat().st_mode & 0o777) == 0o700
    monkeypatch.delenv("XDG_RUNTIME_DIR")
    fallback = remote.control_directory()
    assert str(fallback) == f"/tmp/fridica-ssh-{os.getuid()}" and len(str(fallback)) + 41 < 100
    (tmp_path / "loose").mkdir(mode=0o755)
    monkeypatch.setenv("XDG_RUNTIME_DIR", str(tmp_path / "loose"))
    (tmp_path / "loose" / "fridica").mkdir(mode=0o755)
    with pytest.raises(OSError):
        remote.control_directory()


def test_remote_script_quotes_everything_and_cleans_up(tmp_path):
    directory = PurePosixPath(tmp_path / "scratch $(touch INJECTED)")
    body = '{"a":"it\'s $HOME `x` \\\\ \\"q\\""}'
    script = remote.remote_script(["sh", "-c", f"cat '{directory}/schema.json'; pwd; cat"], PurePosixPath(tmp_path),
                                  files={"schema.json": body}, directory=directory, env={"OMP_NUM_THREADS": "4"}, timeout=5)
    assert script.startswith("exec sh -c ")
    for shell in ("bash", "sh"):
        result = subprocess.run([shell, "-c", script], input="PROMPT", capture_output=True, text=True)
        assert result.returncode == 0, result.stderr
        assert result.stdout == body + str(tmp_path) + "\nPROMPT"
    assert not os.path.exists(directory) and not (tmp_path / "INJECTED").exists()
    failing = remote.remote_script(["sh", "-c", "exit 7"], PurePosixPath(tmp_path))
    assert subprocess.run(["sh", "-c", failing]).returncode == 7
    assert subprocess.run(["sh", "-c", remote.remote_script(["true"], PurePosixPath(tmp_path / "missing"))]).returncode == 98


def test_launch_is_identity_for_local(config, tmp_path):
    assert remote.launch(config, ["codex", "exec"], config.workspace) == (["codex", "exec"], config.workspace)


@pytest.mark.parametrize("backend_type,name", [(ClaudeBackend, "claude"), (CodexBackend, "codex")])
def test_remote_roundtrip_through_ssh(remote_config, tmp_path, monkeypatch, message, fake_ssh, backend_type, name):
    executable = tmp_path / "bin" / name
    executable.write_text(f'#!{sys.executable}\n' + '''import json, os, pathlib, sys
arguments = sys.argv[1:]
prompt = sys.stdin.read()
assert "Conversation data:" in prompt
assert not any("SLACK" in key for key in os.environ)
classification = ("--tools" in arguments and arguments[arguments.index("--tools") + 1] == "") or ("--sandbox" in arguments and arguments[arguments.index("--sandbox") + 1] == "read-only")
pathlib.Path(os.environ["CWD_LOG"]).open("a").write(os.getcwd() + "\\n")
result = {"decision": "respond"} if classification else {"text": "Finished remotely", "status": "complete"}
if "--output-schema" in arguments:
    schema_path = pathlib.Path(arguments[arguments.index("--output-schema") + 1])
    assert schema_path.is_file() and "properties" in json.loads(schema_path.read_text())
    assert str(schema_path).startswith("/tmp/fridica-agent-")
    print(json.dumps({"type": "thread.started", "thread_id": "0193b2c4-1111-7000-8000-000000000000"}))
    print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": json.dumps(result)}}))
else:
    json.loads(arguments[arguments.index("--json-schema") + 1])
    print(json.dumps({"structured_output": result, "session_id": "0193b2c4-1111-7000-8000-000000000000"}))
''')
    executable.chmod(0o700)
    cwd_log = tmp_path / "cwd.log"
    monkeypatch.setenv("CWD_LOG", str(cwd_log))
    monkeypatch.setenv(remote_config.app_token_env, "xapp-secret")
    backend = backend_type(remote_config)
    context = ConversationContext([], remote_config.owner_id, "profile", "task", 1)
    assert asyncio.run(backend.classify(message(), context)) == Decision.RESPOND
    result = asyncio.run(backend.respond(message(), context))
    assert result.text == "Finished remotely" and result.status == "complete"
    assert result.session == "0193b2c4-1111-7000-8000-000000000000"
    classify_cwd, respond_cwd = cwd_log.read_text().splitlines()
    assert classify_cwd.startswith("/tmp/fridica-agent-") and not os.path.exists(classify_cwd)
    assert respond_cwd == str(remote_config.workspace)
    calls = [json.loads(line) for line in fake_ssh.read_text().splitlines()]
    assert len(calls) == 2 and all(call["host"] == "dart9" for call in calls)
    assert all(call["script"].startswith("exec sh -c ") for call in calls)
    assert f"cd {remote_config.workspace}" in calls[1]["script"] or f"cd '{remote_config.workspace}'" in calls[1]["script"]
    assert not list(tmp_path.glob("fridica-agent-*"))


def test_ssh_connection_failure_is_reported(remote_config, fake_ssh, monkeypatch):
    monkeypatch.setenv("SSH_FAIL", "1")
    argv, cwd = remote.launch(remote_config, ["codex", "exec", "-"], remote_config.workspace)
    with pytest.raises(BackendError) as info:
        asyncio.run(_run(argv, "prompt", cwd, remote_config))
    assert "dart9" in str(info.value) and "255" in str(info.value)
    assert "Connection refused" in str(info.value)


def test_remote_checks_go_through_ssh(remote_config, monkeypatch):
    calls = []

    def run(command, **kwargs):
        calls.append(command)
        assert command[0] == "ssh" and command[command.index("--") + 1] == "dart9"
        assert remote_config.user_token_env not in kwargs["env"]
        script = command[-1]
        if "uname -s" in script:
            return subprocess.CompletedProcess(command, 0, "Linux\n", "")
        if "--help" in script:
            flags = " ".join(["--ignore-user-config", "--ignore-rules", "--output-schema", "--ephemeral", "resume",
                              "--setting-sources", "--strict-mcp-config", "--json-schema", "dontAsk", "acceptEdits",
                              "--session-id", "--resume"])
            return subprocess.CompletedProcess(command, 0, flags, "")
        if "auth status" in script:
            return subprocess.CompletedProcess(command, 0, '{"loggedIn":true}', "")
        if "login status" in script:
            return subprocess.CompletedProcess(command, 0, "", "")
        if "--unshare-user" in script:
            return subprocess.CompletedProcess(command, 0, "", "")
        if "test -d" in script:
            return subprocess.CompletedProcess(command, 0, "", "")
        if "command -v" in script:
            # Every script also probes for the `timeout` wrapper; the agent's own command comes last.
            name = re.findall(r"command -v ([A-Za-z0-9_-]+)", script)[-1]
            return subprocess.CompletedProcess(command, 0, f"/remote/bin/{name}\n", "")
        raise AssertionError(script)

    monkeypatch.setattr(agents.subprocess, "run", run)
    monkeypatch.setattr(agents.shutil, "which", lambda name: pytest.fail("local which must not be used"))
    monkeypatch.setenv(remote_config.user_token_env, "xoxp-secret")
    remote_config = replace(remote_config, additional_workspaces=(PurePosixPath("/mnt/data"),))
    assert checks.check_connection(remote_config) == []
    assert checks.which(remote_config, "codex") == "/remote/bin/codex"
    assert checks.platform(remote_config) == "linux"
    for backend in ("claude", "codex"):
        current = replace(remote_config, backend=backend)
        assert checks.check_backend(current) == []
        assert checks.check_sandbox(current) == []
        assert checks.check_authentication(current) == []
    assert all(command[1] == "-T" for command in calls)
    assert any("test -d /mnt/data" in command[-1] for command in calls)


def test_remote_connection_problems(config, remote_config, monkeypatch):
    def refused(command, **kwargs):
        return subprocess.CompletedProcess(command, 255, "", "ssh: connect to host dart9 port 22: Connection refused")

    monkeypatch.setattr(agents.subprocess, "run", refused)
    (problem,) = checks.check_connection(remote_config)
    assert "dart9" in problem and "Connection refused" in problem and "~/.ssh/config" in problem
    assert checks.which(remote_config, "codex") is None
    assert checks.platform(remote_config) is None
    assert "dart9" in checks.check_backend(remote_config)[0]

    monkeypatch.setattr(agents.subprocess, "run", lambda command, **kwargs: subprocess.CompletedProcess(command, 98, "", "cd: no such directory"))
    (problem,) = checks.check_connection(remote_config)
    assert "is not a directory on dart9" in problem

    monkeypatch.setattr(agents.subprocess, "run", lambda command, **kwargs: subprocess.CompletedProcess(command, 0, "FreeBSD\n", ""))
    (problem,) = checks.check_connection(remote_config)
    assert "FreeBSD" in problem

    def timeout(command, **kwargs):
        raise subprocess.TimeoutExpired(command, 30)

    monkeypatch.setattr(agents.subprocess, "run", timeout)
    assert "timed out" in checks.check_connection(remote_config)[0]
    assert checks.check_connection(config) == []


def test_doctor_reports_ssh_connection(remote_config, monkeypatch, capsys):
    from fridica import doctor
    from fridica.cli import main
    monkeypatch.setattr(doctor, "load_config", lambda path: remote_config)
    monkeypatch.setenv(remote_config.app_token_env, "xapp-secret")
    monkeypatch.setenv(remote_config.user_token_env, "xoxp-secret")
    monkeypatch.setattr(doctor, "check_connection", lambda config: ["Could not connect to dart9 without a prompt."])
    monkeypatch.setattr(doctor, "which", lambda config, name: pytest.fail("must not probe the backend"))
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "FAIL SSH connection (dart9, workspace dart9:" in output
    assert output.count("SKIP ") == 4 and "fix the SSH connection first" in output
    monkeypatch.setattr(doctor, "check_connection", lambda config: [])
    monkeypatch.setattr(doctor, "which", lambda config, name: "/remote/bin/" + name)
    for name in ("check_backend", "check_sandbox", "check_authentication"):
        monkeypatch.setattr(doctor, name, lambda config: [])
    assert main(["doctor"]) == 0
    output = capsys.readouterr().out
    assert "PASS SSH connection (dart9" in output and "PASS AI executable (claude, on dart9)" in output
    assert "11 passed, 0 failed, 0 skipped" in output


def test_remote_command_failure_is_distinguished_from_missing_directory(remote_config, monkeypatch):
    monkeypatch.setattr(agents.subprocess, "run",
                        lambda command, **kwargs: subprocess.CompletedProcess(command, 1, "", "bash: uname: command not found"))
    (problem,) = checks.check_connection(remote_config)
    assert "status 1" in problem and "command not found" in problem and "not a directory" not in problem
