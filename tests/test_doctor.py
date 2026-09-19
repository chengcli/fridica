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
    monkeypatch.setattr(doctor, "check_authentication", lambda config: [])
    monkeypatch.setenv(config.app_token_env, "xapp-secret")
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")


def test_doctor_all_pass(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    assert main(["doctor"]) == 0
    output = capsys.readouterr().out
    assert output.count("PASS ") == 7
    assert "PASS AI sign-in" in output
    assert "7 passed, 0 failed, 0 skipped" in output
    assert "secret" not in output


def test_doctor_reports_all_independent_failures(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    monkeypatch.delenv(config.app_token_env)
    monkeypatch.delenv(config.user_token_env)
    monkeypatch.setattr(doctor, "check_backend", lambda config: ["missing flags"])
    monkeypatch.setattr(doctor, "check_authentication", lambda config: ["not signed in"])
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "PASS Configuration" in output
    for label in ("Slack app token format", "Slack user token format", "AI CLI capabilities", "AI sign-in"):
        assert f"FAIL {label}" in output
    assert "3 passed, 4 failed, 0 skipped" in output


def test_doctor_missing_executable_skips_dependent_checks(config, monkeypatch, capsys):
    mock_doctor(config, monkeypatch)
    monkeypatch.setattr(doctor.shutil, "which", lambda name: None)
    monkeypatch.setattr(doctor, "check_authentication", lambda config: pytest.fail("must not run"))
    assert main(["doctor"]) == 1
    output = capsys.readouterr().out
    assert "FAIL AI executable" in output
    assert "SKIP AI sign-in" in output
    assert "4 passed, 1 failed, 2 skipped" in output


def test_doctor_invalid_configuration(tmp_path, capsys):
    assert main(["doctor", "--config", str(tmp_path / "missing.toml")]) == 1
    output = capsys.readouterr().out
    assert "FAIL Configuration" in output
    assert output.count("SKIP ") == 5
