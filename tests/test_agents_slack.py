import asyncio
from dataclasses import replace
import json
import os
import sys

import pytest
from slack_sdk.errors import SlackApiError

from fridica.agents import BackendError, ClaudeBackend, CodexBackend, _run
from fridica.models import AgentResult, ConversationContext, Decision
from fridica.replica import DeliveryRejected, RateLimited
from fridica.slack import MARKER, SlackTransport, normalize


@pytest.fixture
def payload():
    return {"type": "event_callback", "event_id": "evt", "team_id": "TTEAM", "event": {
        "type": "message", "channel": "CROOM", "user": "UALICE", "text": "hello", "ts": "100.000001",
    }}


def test_normalize_metadata_and_fallback(payload):
    payload["event"]["metadata"] = {"event_type": "fridica_message", "event_payload": {"task_id": "task", "turn": 2}}
    entry = normalize(payload)
    assert entry.generated and entry.task_id == "task" and entry.turn == 2
    payload["event"]["metadata"] = []
    payload["event"]["text"] += MARKER
    assert normalize(payload).generated


@pytest.mark.parametrize("changes", [{"subtype": "message_changed"}, {"subtype": []}, {"bot_id": "B1"}, {"ts": "nan"}, {"thread_ts": []}, {"user": None}])
def test_normalize_rejects_invalid_events(payload, changes):
    payload["event"].update(changes)
    assert normalize(payload) is None


def test_malformed_metadata_cannot_crash(payload):
    payload["event"]["metadata"] = {"event_type": "fridica_message", "event_payload": {"turn": [], "task_id": [], "status": []}}
    entry = normalize(payload)
    assert entry.turn == 0 and entry.task_id is None


class Client:
    def __init__(self):
        self.identity = {"user_id": "UOWNER", "team_id": "TTEAM"}
        self.member = True
        self.error = None
        self.post = None

    async def auth_test(self):
        return self.identity

    async def conversations_info(self, **kwargs):
        return {"channel": {"is_member": self.member}}

    async def chat_postMessage(self, **kwargs):
        self.post = kwargs
        if self.error:
            raise self.error
        return {"ts": "101.000001"}


def test_slack_identity_membership_and_reply(config, message):
    client = Client()
    transport = SlackTransport(config, client)
    asyncio.run(transport.validate())
    asyncio.run(transport.send(message(), AgentResult("done"), "task", 2))
    assert client.post["thread_ts"] == message().thread_id
    assert client.post["text"].endswith(MARKER)
    assert client.post["metadata"]["event_payload"]["turn"] == 2
    client.identity["user_id"] = "UOTHER"
    with pytest.raises(ValueError):
        asyncio.run(transport.validate())
    client.identity["user_id"] = "UOWNER"
    client.member = False
    with pytest.raises(ValueError):
        asyncio.run(transport.validate())


@pytest.mark.parametrize("status,error_type", [(429, RateLimited), (500, RuntimeError), (403, DeliveryRejected)])
def test_slack_delivery_errors(config, message, status, error_type):
    class Response(dict):
        status_code = status
        headers = {"Retry-After": ["2"]}

    client = Client()
    client.error = SlackApiError("failure", Response(error="some_error"))
    with pytest.raises(error_type) as raised:
        asyncio.run(SlackTransport(config, client).send(message(), AgentResult("done"), "task", 1))
    if status == 429:
        assert raised.value.retry_after == 2


@pytest.mark.parametrize("backend_type", [ClaudeBackend, CodexBackend])
def test_backend_permission_flags(config, tmp_path, backend_type):
    backend = backend_type(config)
    schema = tmp_path / "schema.json"
    schema.write_text('{}')
    classify = backend.command(tmp_path, schema, True)
    execute = backend.command(tmp_path, schema, False)
    assert not any("bypass" in part or "dangerously" in part for part in execute)
    if backend_type is ClaudeBackend:
        assert classify[classify.index("--tools") + 1] == ""
        assert execute[execute.index("--permission-mode") + 1] == "dontAsk"
        settings = json.loads(execute[execute.index("--settings") + 1])
        assert settings["sandbox"]["enabled"]
        assert settings["sandbox"]["failIfUnavailable"]
        assert not settings["sandbox"]["allowUnsandboxedCommands"]
        available = set(execute[execute.index("--tools") + 1].split(","))
        allowed = set(execute[execute.index("--allowedTools") + 1].split(","))
        assert not {"Edit", "Write"} & available
        assert not {"Edit", "Write", "Bash"} & allowed
        assert "Bash" in available
        assert settings["sandbox"]["autoAllowBashIfSandboxed"]
    else:
        assert classify[classify.index("--sandbox") + 1] == "read-only"
        assert execute[execute.index("--sandbox") + 1] == "workspace-write"
        assert "features.shell_tool=false" in classify
        assert "sandbox_workspace_write.network_access=false" in execute
        assert "--ignore-user-config" in execute


def test_parsers_fail_closed(config, tmp_path):
    claude = ClaudeBackend(config)
    with pytest.raises(BackendError):
        claude.parse(json.dumps({"permission_denials": ["Write"], "structured_output": {"text": "done"}}), tmp_path)
    with pytest.raises(BackendError):
        claude.parse('{"structured_output": []}', tmp_path)
    with pytest.raises(BackendError):
        CodexBackend(config).parse("", tmp_path)


@pytest.mark.parametrize("backend_type,name", [(ClaudeBackend, "claude"), (CodexBackend, "codex")])
def test_fake_cli_roundtrip_and_token_stripping(config, tmp_path, monkeypatch, message, backend_type, name):
    additional = tmp_path / "project with spaces [glob]* $(touch INJECTED)"
    additional.mkdir()
    config = replace(config, additional_workspaces=(additional,))
    monkeypatch.setenv("EXPECTED_ADDITIONAL_WORKSPACE", str(additional))
    executable = tmp_path / name
    executable.write_text(f'#!{sys.executable}\n' + '''import json, os, pathlib, sys
assert not any("SLACK" in key for key in os.environ)
assert "CUSTOM_SECRET" not in os.environ
prompt = sys.stdin.read()
assert "Conversation data:" in prompt
assert "touch SHOULD_NOT_EXIST" in prompt
arguments = sys.argv[1:]
classification = ("--tools" in arguments and arguments[arguments.index("--tools") + 1] == "") or ("--sandbox" in arguments and arguments[arguments.index("--sandbox") + 1] == "read-only")
if classification:
    assert "--add-dir" not in arguments
else:
    assert arguments[arguments.index("--add-dir") + 1] == os.environ["EXPECTED_ADDITIONAL_WORKSPACE"]
result = {"decision": "respond"} if classification else {"text": "Finished safely", "status": "complete"}
if "--output-last-message" in arguments:
    pathlib.Path(arguments[arguments.index("--output-last-message") + 1]).write_text(json.dumps(result))
    print(json.dumps({"type": "thread.started"}))
else:
    print(json.dumps({"structured_output": result}))
''')
    executable.chmod(0o700)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    monkeypatch.setenv(config.app_token_env, "xapp-secret")
    monkeypatch.setenv(config.user_token_env, "xoxp-secret")
    monkeypatch.setenv("CUSTOM_SECRET", "xoxp-other-secret")
    entry = message(text="$(touch SHOULD_NOT_EXIST)")
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    backend = backend_type(config)
    assert asyncio.run(backend.classify(entry, context)) == Decision.RESPOND
    assert asyncio.run(backend.respond(entry, context)) == AgentResult("Finished safely")
    assert not (config.workspace / "SHOULD_NOT_EXIST").exists()
    assert not (config.workspace / "INJECTED").exists()


def test_subprocess_timeout_and_cancellation(config, tmp_path):
    script = tmp_path / "sleep.py"
    pidfile = tmp_path / "pid"
    script.write_text('import os, pathlib, sys, time\npathlib.Path(sys.argv[1]).write_text(str(os.getpid()))\ntime.sleep(60)\n')
    command = [sys.executable, str(script), str(pidfile)]
    with pytest.raises(TimeoutError):
        asyncio.run(_run(command, "", config.workspace, replace(config, timeout=1)))
    with pytest.raises(ProcessLookupError):
        os.kill(int(pidfile.read_text()), 0)

    async def cancel():
        pidfile.unlink()
        task = asyncio.create_task(_run(command, "", config.workspace, config))
        for attempt in range(200):
            if pidfile.exists():
                break
            await asyncio.sleep(0.01)
        assert pidfile.exists()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        with pytest.raises(ProcessLookupError):
            os.kill(int(pidfile.read_text()), 0)
    asyncio.run(cancel())
