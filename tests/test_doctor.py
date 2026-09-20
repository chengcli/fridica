from dataclasses import replace
from pathlib import Path
import subprocess

import pytest

from fridica import agents, doctor
from fridica.cli import main


@pytest.mark.parametrize("backend,code,output,passed", [
    ("claude", 0, '{"loggedIn":true,"email":"private@example.com"}', True),
    ("claude", 0, '{"loggedIn":false}', False),
    ("claude", 0, '{"loggedIn":"true"}', False),
    ("claude", 0, '[]', False),
    ("claude", 0, 'broken JSON', False),
    ("claude", 1, 'native binary not installed', False),
    ("codex", 0, '', True),
    ("codex", 1, '', False),
])
def test_authentication_status(config, monkeypatch, backend, code, output, passed):
    config = replace(config, backend=backend)
    monkeypatch.setattr(agents.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setenv(config.user_token_env, "xoxp-private-token")

    def run(command, **kwargs):
        expected = ["auth", "status"] if backend == "claude" else ["login", "status"]
        assert command == [f"/bin/{backend}", *expected]
        assert kwargs["stdin"] == subprocess.DEVNULL
        assert kwargs["timeout"] == 10
        assert config.user_token_env not in kwargs["env"]
        return subprocess.CompletedProcess(command, code, output, "private-error")

    monkeypatch.setattr(agents.subprocess, "run", run)
    problems = agents.check_authentication(config)
    assert (not problems) == passed
    assert "private" not in " ".join(problems)


@pytest.mark.parametrize("error", [OSError("secret"), subprocess.TimeoutExpired("claude", 10)])
def test_authentication_command_failure(config, monkeypatch, error):
    monkeypatch.setattr(agents.shutil, "which", lambda name: name)

    def run(*args, **kwargs):
        raise error

    monkeypatch.setattr(agents.subprocess, "run", run)
    problems = agents.check_authentication(config)
    assert problems
    assert "secret" not in problems[0]


def mock_doctor(config, monkeypatch):
    monkeypatch.setattr(doctor, "load_config", lambda path: config)
    monkeypatch.setattr(doctor.sys, "platform", "linux")
    monkeypatch.setattr(doctor.shutil, "which", lambda name: f"/bin/{name}")
    monkeypatch.setattr(doctor, "check_backend", lambda config: [])
    monkeypatch.setattr(doctor, "check_sandbox", lambda config: [])
    monkeypatch.setattr(doctor, "check_authentication", lambda config: [])
    monkeypatch.setenv(config.app_token_env, "xapp-secret")
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")


def test_doctor_all_pass(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    assert main(["doctor"]) == 0
    output = capsys.readouterr().out
    assert output.count("PASS ") == 9
    assert "PASS Agent contract (packaged default)" in output
    assert "PASS AI sandbox" in output
    assert "PASS AI sign-in" in output
    assert "9 passed, 0 failed, 0 skipped" in output
    assert "secret" not in output


def test_doctor_reports_all_independent_failures(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    monkeypatch.delenv(config.app_token_env)
    monkeypatch.delenv(config.user_token_env)
    monkeypatch.setattr(doctor, "check_backend", lambda config: ["missing flags"])
    monkeypatch.setattr(doctor, "check_sandbox", lambda config: ["Install socat"])
    monkeypatch.setattr(doctor, "check_authentication", lambda config: ["not signed in"])
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "PASS Configuration" in output
    for label in ("Slack app token format", "Slack user token format", "AI CLI capabilities",
                  "AI sandbox", "AI sign-in"):
        assert f"FAIL {label}" in output
    assert "FAIL AI sandbox: Install socat" in output
    assert "4 passed, 5 failed, 0 skipped" in output


def test_doctor_missing_executable_skips_dependent_checks(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    monkeypatch.setattr(doctor.shutil, "which", lambda name: None)
    monkeypatch.setattr(doctor, "check_authentication", lambda config: pytest.fail("must not run"))
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "FAIL AI executable" in output
    assert "SKIP AI sandbox" in output
    assert "SKIP AI sign-in" in output
    assert "5 passed, 1 failed, 3 skipped" in output


def test_doctor_invalid_configuration(tmp_path, capsys):
    assert main(["doctor", "--config", str(tmp_path / "missing.toml")]) == 1
    output = capsys.readouterr().out
    assert "FAIL Configuration" in output
    assert output.count("SKIP ") == 7


def test_doctor_reports_broken_contract(config, monkeypatch, capsys, tmp_path):
    path = tmp_path / "contract.md"
    path.write_text("## Replies\n\nonly\n")
    config = replace(config, contract=path)
    mock_doctor(config, monkeypatch)
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "FAIL Agent contract:" in output and "## Participation" in output
    assert "8 passed, 1 failed, 0 skipped" in output


def mock_bwrap(monkeypatch, config, code=0, stderr=""):
    calls = []

    def run(command, **kwargs):
        calls.append(command)
        assert command[0] == "/bin/bwrap" and command[-1] == "/bin/true"
        assert "--unshare-user" in command and "--unshare-net" in command
        assert kwargs["stdin"] == subprocess.DEVNULL and kwargs["timeout"] == 10
        assert config.user_token_env not in kwargs["env"]
        return subprocess.CompletedProcess(command, code, "", stderr)

    monkeypatch.setattr(agents.subprocess, "run", run)
    return calls


@pytest.mark.parametrize("backend,platform,available,problems,probed", [
    ("claude", "linux", {"bwrap", "socat"}, 0, True),
    ("claude", "linux", {"bwrap"}, 1, False),
    ("claude", "linux", set(), 1, False),
    ("claude", "darwin", set(), 0, False),
    ("codex", "linux", {"bwrap"}, 0, True),
    ("codex", "linux", set(), 0, False),
    ("codex", "darwin", set(), 0, False),
])
def test_sandbox_dependencies(config, monkeypatch, backend, platform, available, problems, probed):
    config = replace(config, backend=backend)
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")
    monkeypatch.setattr(agents.sys, "platform", platform)
    monkeypatch.setattr(agents.shutil, "which", lambda name: f"/bin/{name}" if name in available else None)
    calls = mock_bwrap(monkeypatch, config)
    result = agents.check_sandbox(config)
    assert len(result) == problems
    assert bool(calls) == probed
    if problems:
        for tool in {"bwrap", "socat"} - available:
            assert tool in result[0]


@pytest.mark.parametrize("backend", ["claude", "codex"])
def test_sandbox_user_namespace_restriction(config, monkeypatch, backend):
    config = replace(config, backend=backend)
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")
    monkeypatch.setattr(agents.sys, "platform", "linux")
    monkeypatch.setattr(agents.shutil, "which", lambda name: f"/bin/{name}")
    mock_bwrap(monkeypatch, config, 1, "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted\n")
    (problem,) = agents.check_sandbox(config)
    assert "cannot create user namespaces" in problem
    assert "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted" in problem
    assert "apparmor_restrict_unprivileged_userns" in problem


@pytest.mark.parametrize("error", [OSError("secret"), subprocess.TimeoutExpired("bwrap", 10)])
def test_sandbox_probe_failure(config, monkeypatch, error):
    monkeypatch.setattr(agents.sys, "platform", "linux")
    monkeypatch.setattr(agents.shutil, "which", lambda name: f"/bin/{name}")

    def run(*args, **kwargs):
        raise error

    monkeypatch.setattr(agents.subprocess, "run", run)
    (problem,) = agents.check_sandbox(config)
    assert "secret" not in problem
    assert "README" in problem
