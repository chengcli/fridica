import asyncio
from dataclasses import replace

import pytest

from fridica.models import AgentResult, Decision
from fridica.replica import DeliveryRejected, RateLimited, Replica
from fridica.store import Store


class Agent:
    def __init__(self, decision=Decision.RESPOND, results=None):
        self.decision = decision
        self.results = list(results or [AgentResult("done")])
        self.classified = []
        self.responded = []

    async def classify(self, message, context):
        self.classified.append(message)
        return self.decision

    async def respond(self, message, context):
        self.responded.append((message, context))
        return self.results.pop(0)


class Transport:
    def __init__(self, error=None):
        self.error = error
        self.sent = []

    async def send(self, message, result, task_id, turn):
        self.sent.append((message, result, task_id, turn))
        if self.error:
            raise self.error
        return f"{100 + len(self.sent)}.000002"


def process(replica, message):
    replica.receive(message)
    asyncio.run(replica.process(message))


@pytest.mark.parametrize("changes,expected", [
    ({"sender_id": "UOWNER"}, "observe"),
    ({"sender_id": "UOWNER", "generated": True}, "ignore"),
    ({"generated": True, "text": "general question"}, "ignore"),
    ({"turn": 6, "generated": True}, "ignore"),
    ({"generated": True, "task_status": "complete"}, "ignore"),
])
def test_filters(config, store, message, changes, expected):
    agent = Agent()
    replica = Replica(config, store, agent, Transport())
    entry = message(**changes)
    process(replica, entry)
    assert store.get(entry.event_id)["decision"] == expected
    assert not agent.classified and not agent.responded
    assert not replica.receive(message("outside", channel_id="COTHER"))
    assert not replica.receive(message("outside", workspace_id="TOTHER"))


def test_invalid_classification_observes(config, store, message):
    agent = Agent(decision="respond")
    entry = message(text="general question")
    process(Replica(config, store, agent, Transport()), entry)
    assert store.get(entry.event_id)["state"] == "observed"
    assert not agent.responded


def test_observe_only_never_invokes_or_posts(config, store, message):
    transport = Transport()
    entry = message()
    process(Replica(config, store, None, transport, observe_only=True), entry)
    assert store.get(entry.event_id)["state"] == "observed"
    assert not transport.sent


def test_observe_mode_retains_pending_delivery(config, store, message):
    entry = message()
    replica = Replica(config, store, Agent(), Transport(RateLimited(1)))
    process(replica, entry)
    process(Replica(config, store, None, Transport(), observe_only=True), entry)
    assert store.get(entry.event_id)["state"] == "ready"


def test_undelivered_clarification_defers_followup(config, store, message):
    agent = Agent(results=[AgentResult("Which file?", "waiting"), AgentResult("Created file")])
    transport = Transport(RateLimited(1))
    replica = Replica(config, store, agent, transport)
    original = message()
    process(replica, original)
    entry = message("followup", timestamp="102.000001")
    process(replica, entry)
    assert len(agent.responded) == 1
    assert store.get(entry.event_id)["state"] == "pending"
    transport.error = None
    process(replica, original)
    process(replica, entry)
    process(replica, entry)
    assert len(agent.responded) == 2
    assert len(transport.sent) == 3
    assert store.get(original.event_id)["state"] == "sent"
    assert store.get(entry.event_id)["state"] == "sent"


def test_clarification_creates_file_and_survives_restart(config, message):
    class FileAgent(Agent):
        async def respond(self, entry, context):
            self.responded.append((entry, context))
            assert any(item.text == "<@UALICE> Which filename?" for item in context.messages)
            (config.workspace / "answer.txt").write_text(entry.text)
            return AgentResult("Created answer.txt")

    database = Store(config.state_path)
    first = message(text="<@UOWNER> create a file")
    agent = Agent(results=[AgentResult("Which filename?", "waiting")])
    initial_transport = Transport()
    process(Replica(config, database, agent, initial_transport), first)
    assert initial_transport.sent[0][1].text == "<@UALICE> Which filename?"
    task_id = database.task(first)["task_id"]
    database.close()
    database = Store(config.state_path)
    try:
        next_agent = FileAgent()
        transport = Transport()
        second = message("second", text="answer.txt", timestamp="102.000001")
        replica = Replica(config, database, next_agent, transport)
        process(replica, second)
        process(replica, second)
        assert (config.workspace / "answer.txt").read_text() == "answer.txt"
        assert len(next_agent.responded) == len(transport.sent) == 1
        assert database.task(second)["status"] == "complete"
        assert database.task(second)["task_id"] == task_id
        assert database.task(second)["turns"] == 2
    finally:
        database.close()


@pytest.mark.parametrize("error,state", [(RateLimited(1), "ready"), (RuntimeError("unknown"), "ambiguous"), (DeliveryRejected(), "failed")])
def test_delivery_never_repeats_workspace_actions(config, store, message, error, state):
    agent = Agent()
    transport = Transport(error)
    replica = Replica(config, store, agent, transport)
    entry = message()
    process(replica, entry)
    assert store.get(entry.event_id)["state"] == state
    transport.error = None
    process(replica, entry)
    assert len(agent.responded) == 1
    assert len(transport.sent) == (2 if state == "ready" else 1)


def test_slack_rejection_logs_code_and_scope_guidance(config, store, message, caplog):
    agent = Agent()
    transport = Transport(DeliveryRejected("missing_scope"))
    entry = message()
    process(Replica(config, store, agent, transport), entry)
    assert "missing_scope" in caplog.text
    assert "chat:write" in caplog.text
    assert store.get(entry.event_id)["state"] == "failed"
    assert len(agent.responded) == len(transport.sent) == 1


def test_turn_limit_persists_without_metadata(config, message):
    config = replace(config, max_turns=1)
    database = Store(config.state_path)
    process(Replica(config, database, Agent(results=[AgentResult("clarify", "waiting")]), Transport()), message())
    database.close()
    database = Store(config.state_path)
    try:
        agent = Agent()
        entry = message("followup", timestamp="102.000001")
        process(Replica(config, database, agent, Transport()), entry)
        assert not agent.responded
        assert database.get(entry.event_id)["decision"] == "respond"
        assert database.get(entry.event_id)["reply_only"] == 1
        assert database.task(entry)["turns"] == 1
        assert database.task(entry)["status"] == "waiting"
    finally:
        database.close()


def test_cooldown_and_thread_isolation(config, store, message):
    agent = Agent(results=[AgentResult("clarify", "waiting")])
    replica = Replica(config, store, agent, Transport())
    process(replica, message())
    other = message("other", text="unrelated", timestamp="102.000001", thread_id="102.000001")
    process(replica, other)
    assert not agent.classified
    assert len(agent.responded) == 1
    assert store.task(other) is None


@pytest.mark.parametrize("during_delivery", [False, True])
def test_cancellation_records_uncertainty(config, store, message, during_delivery):
    async def scenario():
        entered = asyncio.Event()

        async def cancel_target(*args):
            entered.set()
            await asyncio.Event().wait()

        agent = Agent()
        transport = Transport()
        if during_delivery:
            transport.send = cancel_target
        else:
            agent.respond = cancel_target
        replica = Replica(config, store, agent, transport)
        entry = message()
        replica.receive(entry)
        task = asyncio.create_task(replica.process(entry))
        await entered.wait()
        task.cancel()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert store.get(entry.event_id)["state"] == ("ambiguous" if during_delivery else "interrupted")
    asyncio.run(scenario())
