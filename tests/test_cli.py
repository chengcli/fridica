import asyncio
import json
import os
import sys
import threading

import pytest

from fridica.cli.main import main
from fridica.config import load_config
from fridica.doctor.checks import report, run_checks



def test_init_writes_template_and_contract_once(tmp_path, capsys):
    path = tmp_path / "cfg" / "config.toml"
    assert main(["init", "--config", str(path)]) == 0
    assert "[machines.local]" in path.read_text() and (path.parent / "contract.md").read_text().startswith("# Fridica agent contract")
    assert oct(path.stat().st_mode & 0o777) == "0o600"
    with pytest.raises(FileExistsError):
        main(["init", "--config", str(path)])


def test_configure_sets_ids_before_the_rest_is_complete(tmp_path, capsys):
    path = tmp_path / "cfg" / "config.toml"
    main(["init", "--config", str(path)])
    assert main(["configure", "--config", str(path), "--owner-id", "U0ME", "--workspace-id", "T0US",
                 "--channel-id", "C0ONE", "--channel-id", "C0TWO"]) == 0
    text = path.read_text()
    assert 'slack_user = "U0ME"' in text and 'workspace = "T0US"' in text and '"C0ONE", "C0TWO"' in text
    assert "# a few sentences about you" in text  # comments survive
    assert main(["configure", "--config", str(path), "--owner-id", "nope"]) == 2
    assert "Slack member ID" in capsys.readouterr().err


FAKE_CLI = r'''#!{python}
import json, os, sys
name = os.path.basename(sys.argv[0]); args = sys.argv[1:]
if name == "claude":
    if args == ["--help"]:
        print("--input-format --permission-prompts --json-schema --setting-sources --strict-mcp-config --append-system-prompt --session-id dontAsk"
              + (' "auto"' if os.environ.get("CLAUDE_AUTO") else ""))
    elif args[:2] == ["auth", "status"]:
        print(json.dumps({"loggedIn": os.environ.get("CLAUDE_LOGGED_IN") == "1"}))
    sys.exit(0)
if name == "codex":
    if args[:1] == ["login"]:
        sys.exit(0)
    if args[:2] == ["exec", "--help"]:
        print("--ignore-user-config --ignore-rules --output-schema --ephemeral"); sys.exit(0)
    if args[:2] == ["app-server", "generate-json-schema"]:
        out = args[args.index("--out") + 1]
        open(os.path.join(out, "schema.json"), "w").write('"turn/interrupt" "item/commandExecution/requestApproval" "outputSchema"'
                                                          + (' "auto_review"' if os.environ.get("CODEX_AUTO") else ""))
        sys.exit(0)
if name in ("bwrap", "socat"):
    sys.exit(0)
sys.exit(1)
'''


@pytest.fixture
def fake_clis(tmp_path, monkeypatch):
    binaries = tmp_path / "clis"
    binaries.mkdir()
    for name in ("claude", "codex", "bwrap", "socat"):
        path = binaries / name
        path.write_text(FAKE_CLI.replace("{python}", sys.executable))
        path.chmod(0o700)
    monkeypatch.setenv("PATH", str(binaries) + os.pathsep + os.environ["PATH"])
    monkeypatch.setenv("SLACK_APP_TOKEN", "xapp-1")
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxp-1")
    monkeypatch.setenv("CLAUDE_AUTO", "1")  # current CLIs support auto approvals, the default
    monkeypatch.setenv("CODEX_AUTO", "1")


def test_doctor_checks_every_machine(write_config, fake_clis, fake_ssh, monkeypatch, tmp_path, capsys):
    remote = tmp_path / "remote-work"
    remote.mkdir()
    path = write_config(machines=f"""
        [machines.dart9]
        host = "dart9"
        backends = ["codex"]
        [machines.dart9.workspaces]
        canoe = "{remote}"
        missing = "/definitely/not/here"

        [machines.gl]
        transport = "slurm"
        host = "gl"
        slurm = {{ partition = "gpu" }}
        [machines.gl.workspaces]
        scratch = "/scratch"
    """)
    checks = asyncio.run(run_checks(path))
    by_name = {check.name: check for check in checks}
    assert by_name["Configuration (" + str(path.resolve()) + ")"].status == "PASS"
    assert any(check.status == "FAIL" and "not signed in" in check.detail for check in checks)  # claude logged out
    assert any(check.name.startswith("Machine dart9") and check.status == "PASS" for check in checks)
    assert any("workspace missing" in check.name and check.status == "FAIL" for check in checks)
    assert any("workspace canoe" in check.name and check.status == "PASS" for check in checks)
    assert any("app-server protocol" in check.name and check.status == "PASS" for check in checks)
    assert any(check.name.startswith("Machine gl") and check.status == "SKIP" for check in checks)
    monkeypatch.setenv("CLAUDE_LOGGED_IN", "1")
    checks = [check for check in asyncio.run(run_checks(path)) if "missing" not in check.name]
    assert report(checks) == 0
    assert "failed" in capsys.readouterr().out


def test_doctor_reports_unreachable_hosts_and_bad_config(write_config, fake_clis, fake_ssh, monkeypatch, tmp_path):
    path = write_config(machines="""
        [machines.far]
        host = "far"
        [machines.far.workspaces]
        w = "/w"
    """)
    monkeypatch.setenv("SSH_FAIL", "1")
    checks = asyncio.run(run_checks(path))
    assert any(check.name.startswith("Machine far") and "without a prompt" in check.detail for check in checks)
    broken = tmp_path / "broken.toml"
    broken.write_text("[owner]\n")
    checks = asyncio.run(run_checks(broken))
    assert [check.status for check in checks][-2:] == ["FAIL", "SKIP"]


def test_control_commands_talk_to_the_daemon(write_config, monkeypatch, capsys, tmp_path):
    from fridica.control.api import serve
    from test_control_api import Controls
    from fridica.store import Store
    path = write_config()
    config = load_config(path)
    ready = threading.Event()
    stopping = threading.Event()

    def server():
        async def body():
            store = Store(config.state.path)
            runner = await serve(Controls(config, store), config.state.control_socket)
            ready.set()
            while not stopping.is_set():
                await asyncio.sleep(0.02)
            await runner.cleanup()
            store.close()
        asyncio.run(body())

    thread = threading.Thread(target=server)
    thread.start()
    ready.wait(5)
    try:
        assert main(["status", "--config", str(path)]) == 0
        assert json.loads(capsys.readouterr().out)["owner"] == "UOWNER"
        assert main(["approvals", "--config", str(path), "nope", "once"]) == 4
    finally:
        stopping.set()
        thread.join(5)
    assert main(["status", "--config", str(path)]) == 3
    assert "fridica is not" in capsys.readouterr().err


def test_doctor_checks_auto_approval_support(write_config, fake_clis, monkeypatch, tmp_path):
    (tmp_path / "boxw").mkdir()
    path = write_config(machines=f"""
        [machines.box]
        transport = "local"
        backends = ["claude", "codex"]
        policy = {{ approvals = "auto" }}
        [machines.box.workspaces]
        w = "{tmp_path / 'boxw'}"
    """)
    monkeypatch.setenv("CLAUDE_LOGGED_IN", "1")
    monkeypatch.delenv("CLAUDE_AUTO")
    monkeypatch.delenv("CODEX_AUTO")
    failures = [check.detail for check in asyncio.run(run_checks(path)) if check.status == "FAIL"]
    assert any("--permission-mode auto" in detail for detail in failures)
    assert any("auto_review" in detail for detail in failures)
    monkeypatch.setenv("CLAUDE_AUTO", "1")
    monkeypatch.setenv("CODEX_AUTO", "1")
    assert [check for check in asyncio.run(run_checks(path)) if check.status == "FAIL"] == []
