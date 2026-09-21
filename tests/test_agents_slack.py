import asyncio
from dataclasses import replace
import json
import logging
import os
import sys

import pytest
from slack_sdk.errors import SlackApiError

from fridica.agents import BackendError, ClaudeBackend, CodexBackend, SessionUnavailable, _run
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
    assert client.post["text"] == "done"
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
    elif error_type is DeliveryRejected:
        assert raised.value.code == "some_error"


@pytest.mark.parametrize("code,expected", [
    ("missing_scope", "missing_scope"),
    ("invalid_metadata", "invalid_metadata"),
    ("xoxp-secret", "unknown_error"),
    ("bad\nsecret", "unknown_error"),
    (None, "unknown_error"),
])
def test_delivery_error_code_sanitized(code, expected):
    assert str(DeliveryRejected(code)) == expected


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
        assert execute[execute.index("--permission-mode") + 1] == "acceptEdits"
        assert classify[classify.index("--permission-mode") + 1] == "dontAsk"
        settings = json.loads(execute[execute.index("--settings") + 1])
        assert settings["sandbox"]["enabled"]
        assert settings["sandbox"]["failIfUnavailable"]
        assert not settings["sandbox"]["allowUnsandboxedCommands"]
        available = set(execute[execute.index("--tools") + 1].split(","))
        allowed = set(execute[execute.index("--allowedTools") + 1].split(","))
        assert {"Edit", "Write"} <= available
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
print("PRIVATE TOOL DIAGNOSTIC", file=sys.stderr)
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
    print(json.dumps({"type": "thread.started", "diagnostic": "PRIVATE PROGRESS"}))
else:
    print(json.dumps({"structured_output": result, "result": "PRIVATE PROGRESS"}))
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


@pytest.mark.parametrize("backend_type", [ClaudeBackend, CodexBackend])
def test_session_flags(config, tmp_path, backend_type):
    schema = tmp_path / "schema.json"
    schema.write_text("{}")
    extra = tmp_path / "extra"
    extra.mkdir()
    config = replace(config, additional_workspaces=(extra,))
    backend = backend_type(config)
    session = "2b1f0d2e-6a8e-4c39-9a33-0d8c9a0f1b22"
    classify = backend.command(tmp_path, schema, True, None, False)
    fresh = backend.command(tmp_path, schema, False, session, False)
    resumed = backend.command(tmp_path, schema, False, session, True)
    stateless = backend.command(tmp_path, schema, False, None, False)
    for command in (classify, fresh, resumed, stateless):
        assert not any("bypass" in part or "dangerously" in part for part in command)
    if backend_type is ClaudeBackend:
        assert "--no-session-persistence" in classify and "--no-session-persistence" in stateless
        assert fresh[fresh.index("--session-id") + 1] == session and "--no-session-persistence" not in fresh
        assert resumed[resumed.index("--resume") + 1] == session and "--session-id" not in resumed
        for command in (fresh, resumed):
            assert command[command.index("--permission-mode") + 1] == "acceptEdits"
            assert command[command.index("--add-dir") + 1] == str(extra)
            assert json.loads(command[command.index("--settings") + 1])["sandbox"]["failIfUnavailable"]
    else:
        assert "--ephemeral" in classify and "--ephemeral" in stateless
        assert "--ephemeral" not in fresh and fresh[:2] == ["codex", "exec"] and "resume" not in fresh
        assert resumed[:4] == ["codex", "exec", "resume", session]
        assert "--ephemeral" not in resumed and "--sandbox" not in resumed and "--add-dir" not in resumed
        assert 'sandbox_mode="workspace-write"' in resumed
        assert f"sandbox_workspace_write.writable_roots={json.dumps([str(extra)])}" in resumed
        for command in (fresh, resumed):
            assert "sandbox_workspace_write.network_access=false" in command
            assert "--ignore-user-config" in command and command[-1] == "-"


@pytest.mark.parametrize("item_type", ["error", "command_execution", "file_change", "mcp_tool_call", "web_search", "unknown"])
def test_codex_classifier_distinguishes_diagnostics_from_tools(config, monkeypatch, item_type):
    from fridica import agents

    async def run(command, prompt, cwd, settings):
        (cwd / "result.json").write_text(json.dumps({"decision": "respond"}))
        return json.dumps({"type": "item.completed", "item": {"type": item_type, "message": "Code mode is disabled."}})

    monkeypatch.setattr(agents, "_run", run)
    backend = CodexBackend(replace(config, backend="codex"))
    if item_type == "error":
        result, session = asyncio.run(backend._invoke("x", True))
        assert result == {"decision": "respond"} and session is None
    else:
        with pytest.raises(BackendError, match="attempted to use tools"):
            asyncio.run(backend._invoke("x", True))


def test_session_id_extraction(config):
    claude = ClaudeBackend(config)
    assert claude.session_id(json.dumps({"session_id": "abcd1234-0000-4000-8000-000000000000"})) == "abcd1234-0000-4000-8000-000000000000"
    assert claude.session_id(json.dumps({"session_id": "bad id; rm -rf"})) is None
    assert claude.session_id("not json") is None
    codex = CodexBackend(config)
    output = "garbage\n" + json.dumps({"type": "thread.started", "thread_id": "0193b2c4-1111-7000-8000-000000000000"}) + "\n" + json.dumps({"type": "turn.completed"})
    assert codex.session_id(output) == "0193b2c4-1111-7000-8000-000000000000"
    assert codex.session_id(json.dumps({"type": "turn.completed"})) is None


@pytest.mark.parametrize("backend_type,name", [(ClaudeBackend, "claude"), (CodexBackend, "codex")])
def test_session_continuity_roundtrip(config, tmp_path, monkeypatch, message, backend_type, name):
    log = tmp_path / "argv.log"
    executable = tmp_path / name
    executable.write_text(f'#!{sys.executable}\n' + '''import json, os, pathlib, sys
arguments = sys.argv[1:]
pathlib.Path(os.environ["ARGV_LOG"]).open("a").write(json.dumps(arguments) + "\\n")
prompt = sys.stdin.read()
resume_id = None
if "--resume" in arguments:
    resume_id = arguments[arguments.index("--resume") + 1]
elif arguments[:2] == ["exec", "resume"]:
    resume_id = arguments[2]
if resume_id == "11111111-1111-4111-8111-111111111111":
    print("No conversation found with session ID: " + resume_id if "--resume" in arguments else "Error: thread/resume failed: no rollout found for thread id " + resume_id, file=sys.stderr)
    sys.exit(1)
session = resume_id or ("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa" if "--session-id" not in arguments else arguments[arguments.index("--session-id") + 1])
result = {"text": "continued" if resume_id else "started", "status": "complete"}
if "--output-last-message" in arguments:
    pathlib.Path(arguments[arguments.index("--output-last-message") + 1]).write_text(json.dumps(result))
    print(json.dumps({"type": "thread.started", "thread_id": session}))
    print(json.dumps({"type": "turn.completed"}))
else:
    print(json.dumps({"structured_output": result, "session_id": session}))
''')
    executable.chmod(0o700)
    monkeypatch.setenv("ARGV_LOG", str(log))
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    backend = backend_type(config)
    entry = message()

    first = asyncio.run(backend.respond(entry, ConversationContext([], config.owner_id, "profile", "task", 1)))
    assert first.text == "started" and first.session
    fresh_argv = json.loads(log.read_text().splitlines()[-1])
    assert "--no-session-persistence" not in fresh_argv and "--ephemeral" not in fresh_argv
    if name == "claude":
        assert fresh_argv[fresh_argv.index("--session-id") + 1] == first.session

    second = asyncio.run(backend.respond(entry, ConversationContext([], config.owner_id, "profile", "task", 2, session=first.session)))
    assert second == AgentResult("continued", "complete", first.session)
    resumed_argv = json.loads(log.read_text().splitlines()[-1])
    assert first.session in resumed_argv and ("--resume" in resumed_argv or resumed_argv[:2] == ["exec", "resume"])

    lost = "11111111-1111-4111-8111-111111111111"
    third = asyncio.run(backend.respond(entry, ConversationContext([], config.owner_id, "profile", "task", 3, session=lost)))
    assert third.text == "started" and third.status == "complete"
    assert third.session and third.session != lost
    assert len(log.read_text().splitlines()) == 4

    stateless = backend_type(replace(config, resume_sessions=False))
    plain = asyncio.run(stateless.respond(entry, ConversationContext([], config.owner_id, "profile", "task", 4, session=first.session)))
    assert plain == AgentResult("started")
    plain_argv = json.loads(log.read_text().splitlines()[-1])
    assert ("--no-session-persistence" in plain_argv) or ("--ephemeral" in plain_argv)
    assert first.session not in plain_argv

    with pytest.raises(BackendError):
        asyncio.run(backend._invoke("x", False, "not a valid session id!"))


def test_resume_failure_is_distinguished(config, tmp_path):
    script = tmp_path / "fail.py"
    script.write_text("import sys\nprint(sys.argv[1], file=sys.stderr)\nsys.exit(1)\n")
    with pytest.raises(SessionUnavailable):
        asyncio.run(_run([sys.executable, str(script), "No conversation found with session ID: x"], "", config.workspace, config))
    with pytest.raises(BackendError) as info:
        asyncio.run(_run([sys.executable, str(script), "some other failure"], "", config.workspace, config))
    assert not isinstance(info.value, SessionUnavailable)


def test_agent_failure_logs_stderr_but_replies_generically(config, tmp_path, monkeypatch, message, caplog):
    executable = tmp_path / "claude"
    executable.write_text(f'#!{sys.executable}\n' + '''import sys
sys.stdin.read()
print("Error: sandbox required but unavailable: socat not installed", file=sys.stderr)
print("  sandbox.failIfUnavailable is set", file=sys.stderr)
sys.exit(1)
''')
    executable.chmod(0o700)
    monkeypatch.setenv("PATH", str(tmp_path) + os.pathsep + os.environ["PATH"])
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    with caplog.at_level(logging.WARNING, logger="fridica.agents"):
        result = asyncio.run(ClaudeBackend(config).respond(message(text="hello"), context))
    assert result.status == "blocked"
    assert "socat" not in result.text
    assert "Agent exited with status 1" in caplog.text
    assert "socat not installed sandbox.failIfUnavailable is set" in caplog.text


def test_diagnostic_is_bounded():
    from fridica.agents import DIAGNOSTIC_LIMIT, _diagnostic
    assert _diagnostic(b"") == ""
    assert _diagnostic(b"a\n b\t\tc\xff") == "a b c\ufffd"
    assert _diagnostic(b"x" * 5000 + b"END") == "x" * (DIAGNOSTIC_LIMIT - 3) + "END"


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
