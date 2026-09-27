import asyncio
import json
import os
import sys

import pytest

from fridica.core.errors import BackendError
from fridica.parent.llm import ClaudeLLM, CodexLLM
from fridica.parent.schemas import TRIAGE_SCHEMA

FAKE_CLAUDE = r'''#!{python}
import json, os, pathlib, sys
arguments = sys.argv[1:]
prompt = sys.stdin.read()
pathlib.Path(os.environ["LLM_LOG"]).write_text(json.dumps({"argv": arguments, "prompt": prompt, "cwd": os.getcwd(),
                                                          "slack": os.environ.get("SLACK_USER_TOKEN")}))
mode = os.environ.get("MODE", "ok")
if mode == "exit":
    print("auth failed", file=sys.stderr); sys.exit(2)
envelope = {"type": "result", "is_error": mode == "error", "result": "oops", "session_id": "s",
            "structured_output": {"decision": "respond"}}
if mode == "tools":
    envelope["permission_denials"] = [{"tool_name": "Bash"}]
print(json.dumps(envelope))
'''

FAKE_CODEX = r'''#!{python}
import json, os, pathlib, sys
arguments = sys.argv[1:]
prompt = sys.stdin.read()
schema = arguments[arguments.index("--output-schema") + 1]
pathlib.Path(os.environ["LLM_LOG"]).write_text(json.dumps({"argv": arguments, "prompt": prompt,
                                                          "schema": json.load(open(schema))}))
mode = os.environ.get("MODE", "ok")
print(json.dumps({"type": "thread.started", "thread_id": "t"}))
if mode == "tools":
    print(json.dumps({"type": "item.completed", "item": {"type": "command_execution", "command": "ls"}}))
print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": json.dumps({"decision": "ignore"})}}))
'''


@pytest.fixture
def fake_llm(tmp_path, monkeypatch):
    binaries = tmp_path / "llm"
    binaries.mkdir()
    for name, body in (("claude", FAKE_CLAUDE), ("codex", FAKE_CODEX)):
        path = binaries / name
        path.write_text(body.replace("{python}", sys.executable))
        path.chmod(0o700)
    monkeypatch.setenv("PATH", str(binaries) + os.pathsep + os.environ["PATH"])
    monkeypatch.setenv("LLM_LOG", str(tmp_path / "llm.json"))
    monkeypatch.setenv("SLACK_USER_TOKEN", "xoxp-secret")
    return lambda: json.loads((tmp_path / "llm.json").read_text())


def test_claude_call_is_tool_less_and_stateless(fake_llm):
    result = asyncio.run(ClaudeLLM(model="m1", excluded_env=("SLACK_USER_TOKEN",)).call("PROMPT", TRIAGE_SCHEMA))
    assert result == {"decision": "respond"}
    log = fake_llm()
    argv = log["argv"]
    assert argv[argv.index("--tools") + 1] == "" and "--no-session-persistence" in argv
    assert json.loads(argv[argv.index("--json-schema") + 1]) == TRIAGE_SCHEMA
    assert argv[argv.index("--model") + 1] == "m1" and argv[argv.index("--permission-mode") + 1] == "dontAsk"
    assert log["prompt"] == "PROMPT" and log["slack"] is None and "fridica-parent-" in log["cwd"]


@pytest.mark.parametrize("mode, message", [("exit", "status 2"), ("error", "reported an error"), ("tools", "tools")])
def test_claude_failures(fake_llm, monkeypatch, mode, message):
    monkeypatch.setenv("MODE", mode)
    with pytest.raises(BackendError, match=message):
        asyncio.run(ClaudeLLM().call("p", TRIAGE_SCHEMA))


def test_codex_call_and_tool_detection(fake_llm, monkeypatch):
    assert asyncio.run(CodexLLM(reasoning_effort="low").call("PROMPT", TRIAGE_SCHEMA)) == {"decision": "ignore"}
    log = fake_llm()
    assert log["schema"] == TRIAGE_SCHEMA and "--ephemeral" in log["argv"] and log["argv"][-1] == "-"
    assert "features.shell_tool=false" in log["argv"] and 'model_reasoning_effort="low"' in log["argv"]
    monkeypatch.setenv("MODE", "tools")
    with pytest.raises(BackendError, match="tools"):
        asyncio.run(CodexLLM().call("p", TRIAGE_SCHEMA))
