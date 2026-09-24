import asyncio
from dataclasses import replace
import json

import pytest

from fridica.agents import ClaudeBackend, CodexBackend, RESPONSE_SCHEMA, _prompt
from fridica.models import AgentResult, ConversationContext
from fridica.replica import RateLimited, Replica
from fridica.replies import format_reply


class Agent:
    def __init__(self, result=AgentResult("Hello UALICE")):
        self.result = result
        self.calls = 0

    async def classify(self, message, context):
        raise AssertionError("Direct mentions must not be classified")

    async def respond(self, message, context):
        self.calls += 1
        if isinstance(self.result, Exception):
            raise self.result
        return self.result


class Transport:
    def __init__(self):
        self.sent = []
        self.error = None

    async def send(self, message, result, task_id, turn):
        if self.error:
            raise self.error
        self.sent.append(result)
        return f"200.{len(self.sent):06d}"


def process(replica, entry):
    replica.receive(entry)
    asyncio.run(replica.process(entry))


def test_names_preserve_markup_and_unknown_ids(config, message):
    entry = message(text="ask <@UBOB>")
    context = ConversationContext([], config.owner_id, "", "task", 1)
    original = "UALICE UBOB UUNKNOWN <@UOWNER> `UALICE` ```UBOB``` https://example.com/UALICE <https://example.com|UBOB>"
    expected = "<@UALICE> <@UBOB> UUNKNOWN <@UOWNER> `UALICE` ```UBOB``` https://example.com/UALICE <https://example.com|UBOB>"
    assert format_reply(original, entry, context) == expected


def test_final_answer_instructions(config, message):
    context = ConversationContext([], config.owner_id, "", "task", 1)
    prompt = _prompt(message(), context, False)
    assert "Fridica delivers your returned text" in prompt
    assert "no workspace action is required" in prompt
    assert "operational diagnostics" in prompt
    assert "owner's first-person voice" in prompt
    assert "address the current sender" in prompt
    assert "Do not append signatures" in prompt
    assert "When ending the conversation (status complete or blocked), do not @mention anyone" in prompt
    assert "Only use Slack <@USER_ID> mentions when status is waiting" in prompt
    assert "If explicitly asked about automation, answer honestly" in prompt
    assert "final user-facing" in RESPONSE_SCHEMA["properties"]["text"]["description"]


def test_legacy_signature_removed(config, message):
    context = ConversationContext([], config.owner_id, "", "task", 1)
    assert format_reply("Hello\n\n[via fridica]", message(), context) == "Hello"
    assert format_reply("Hello [via fridica]  ", message(), context) == "Hello"


@pytest.mark.parametrize("backend_type", [ClaudeBackend, CodexBackend])
def test_only_structured_final_answer(config, message, monkeypatch, backend_type):
    async def run(command, prompt, cwd, config):
        result = {"text": "Hello UALICE", "status": "complete"}
        if "--output-schema" in command:
            return "\n".join([json.dumps({"type": "item.completed", "item": {"type": "reasoning", "text": "PRIVATE DIAGNOSTIC"}}),
                              json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": json.dumps(result)}})])
        return json.dumps({"structured_output": result, "result": "PRIVATE DIAGNOSTIC", "tool_output": "PRIVATE TOOL"})

    monkeypatch.setattr("fridica.agents._run", run)
    context = ConversationContext([], config.owner_id, "", "task", 1)
    result = asyncio.run(backend_type(config).respond(message(), context))
    assert result == AgentResult("Hello UALICE")


def test_mentions_override_unsolicited_settings(config, store, message):
    agent, transport = Agent(), Transport()
    replica = Replica(replace(config, general_messages=False, cooldown=999999), store, agent, transport)
    process(replica, message())
    process(replica, message("second", timestamp="101.000001"))
    assert agent.calls == 2
    assert transport.sent == [AgentResult("Hello <@UALICE>")]
    assert store.get("second")["decision"] == "silent"


@pytest.mark.parametrize("status", ["blocked", "running", "delivery_pending"])
def test_blocker_notice_preserves_task(config, store, message, status):
    entry = message()
    store.add(entry)
    store.begin(entry, "original", 2)
    store.mark(entry.event_id, "failed")
    store.connection.execute("UPDATE tasks SET status=?", (status,))
    store.connection.commit()
    before = dict(store.task(entry))
    agent, transport = Agent(), Transport()
    replica = Replica(config, store, agent, transport)
    followup = message("followup", timestamp="102.000001")
    process(replica, followup)
    assert agent.calls == 0
    assert len(transport.sent) == (0 if status == "blocked" else 1)
    if transport.sent:
        assert "local inspection" in transport.sent[0].text
        assert "<@" not in transport.sent[0].text
    assert dict(store.task(entry)) == before
    process(replica, followup)
    assert len(transport.sent) == (0 if status == "blocked" else 1)


def test_rate_limited_notice_defers_next_mention(config, store, message):
    entry = message()
    store.add(entry)
    store.begin(entry, "original", config.max_turns - 1)
    store.mark(entry.event_id, "failed")
    agent, transport = Agent(), Transport()
    transport.error = RateLimited(1)
    replica = Replica(config, store, agent, transport)
    first = message("first", timestamp="102.000001")
    second = message("second", timestamp="103.000001")
    process(replica, first)
    process(replica, second)
    assert store.get("first")["state"] == "ready"
    assert store.get("second")["state"] == "pending"
    transport.error = None
    process(replica, first)
    process(replica, second)
    assert len(transport.sent) == 1
    assert store.get("second")["decision"] == "blocked"
    assert not agent.calls
    assert store.task(entry)["turns"] == config.max_turns - 1


def test_failure_sends_safe_reply(config, store, message, caplog):
    transport = Transport()
    process(Replica(config, store, Agent(RuntimeError("SECRET")), transport), message())
    assert len(transport.sent) == 1
    assert transport.sent[0].status == "blocked"
    assert "SECRET" not in transport.sent[0].text + caplog.text


class UploadingTransport(Transport):
    def __init__(self):
        super().__init__()
        self.uploads = []

    async def upload(self, message, data, filename):
        self.uploads.append((filename, data.decode("utf-8")))


@pytest.mark.parametrize("status", ["complete", "waiting"])
@pytest.mark.parametrize("text,details", [
    ("\n\n".join(f"Section {index}: " + "word " * 150 for index in range(12)), ""),
    ("UALICE " * 999, "# Notes"),
], ids=["paragraphs", "mentions-with-details"])
def test_overflowing_reply_is_posted_with_the_full_text_attached(config, store, message, status, text, details):
    from fridica.replies import CONTINUED
    transport = UploadingTransport()
    process(Replica(config, store, Agent(AgentResult(text, status, details=details)), transport), message())
    full = format_reply(text, message(), ConversationContext([], config.owner_id, "", "task", 1)).strip()
    [sent] = transport.sent
    assert sent.status == status and len(sent.text) <= 7000 and sent.text.endswith(CONTINUED)
    assert "<@UALICE>" in sent.text if status == "waiting" else sent.text.startswith(full[:20])
    [(name, attached)] = transport.uploads
    assert name.startswith("details-") and attached == sent.details
    assert attached.startswith(full) and attached.endswith(details)
    assert sent.text.removesuffix(CONTINUED).removeprefix("<@UALICE> ") in attached


def test_empty_reply_with_details_points_to_them(config, store, message):
    from fridica.replies import ATTACHED
    transport = UploadingTransport()
    process(Replica(config, store, Agent(AgentResult("  ", details="# Report")), transport), message())
    assert transport.sent[0].text == ATTACHED and transport.uploads[0][1] == "# Report"
