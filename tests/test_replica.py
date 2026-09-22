import asyncio
import json
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

    async def summarize(self, context):
        self.summarized = getattr(self, "summarized", [])
        self.summarized.append(context)
        if getattr(self, "summary_error", None):
            raise self.summary_error
        return "SUMMARY of " + str(len(context.messages)) + " messages"

    async def debrief(self, context):
        self.debriefed = getattr(self, "debriefed", [])
        self.debriefed.append(context)
        if getattr(self, "debrief_error", None):
            raise self.debrief_error
        return "DEBRIEF after " + str(context.turn) + " turns"


class Transport:
    def __init__(self, error=None):
        self.error = error
        self.sent = []
        self.announced = []

    async def send(self, message, result, task_id, turn):
        self.sent.append((message, result, task_id, turn))
        if self.error:
            raise self.error
        return f"{100 + len(self.sent)}.000002"

    async def announce(self, message, text, task_id):
        self.announced.append((message, text, task_id))
        return f"{500 + len(self.announced)}.000009"


def process(replica, message):
    replica.receive(message)
    asyncio.run(replica.process(message))


@pytest.mark.parametrize("changes,expected", [
    ({"sender_id": "UOWNER"}, "observe"),
    ({"sender_id": "UOWNER", "generated": True}, "ignore"),
    ({"generated": True, "text": "general question"}, "ignore"),
    ({"turn": 6, "generated": True}, "ignore"),
    ({"generated": True, "task_status": "complete", "text": "Unsolicited completed response"}, "ignore"),
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
        assert database.get(entry.event_id)["decision"] == "paused"
        assert database.task(entry)["control_state"] == "paused"
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


def test_session_persists_across_turns(config, store, message):
    agent = Agent(results=[
        AgentResult("Which file?", "waiting", session="sess-one"),
        AgentResult("Created file", session="sess-one"),
        AgentResult("Started over", session="sess-two"),
    ])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    process(replica, first)
    assert agent.responded[0][1].session is None
    assert store.task(first)["session"] == "sess-one"
    assert '"session"' not in store.get(first.event_id)["result"] or json.loads(store.get(first.event_id)["result"]).get("session") is None
    assert transport.sent[0][1] == AgentResult("<@UALICE> Which file?", "waiting")

    second = message("event2", text="answer.txt", timestamp="100.000003")
    process(replica, second)
    assert agent.responded[1][1].session == "sess-one"
    assert store.task(second)["session"] == "sess-one"

    third = message("event3", text="<@UOWNER> another", timestamp="100.000005")
    process(replica, third)
    assert agent.responded[2][1].session == "sess-one"
    assert store.task(third)["session"] == "sess-two"


def test_sessions_disabled_never_reach_agent(config, message):
    config = replace(config, resume_sessions=False)
    database = Store(config.state_path)
    try:
        agent = Agent(results=[AgentResult("Which file?", "waiting", session="sess-one"), AgentResult("done")])
        replica = Replica(config, database, agent, Transport())
        first = message()
        process(replica, first)
        assert database.task(first)["session"] is None
        process(replica, message("event2", text="answer.txt", timestamp="100.000003"))
        assert all(context.session is None for _entry, context in agent.responded)
    finally:
        database.close()


def test_idle_session_expires(config, store, message):
    config = replace(config, session_timeout=3600)
    agent = Agent(results=[
        AgentResult("Which file?", "waiting", session="sess-one"),
        AgentResult("Still here", "waiting", session="sess-one"),
        AgentResult("Fresh start", "waiting", session="sess-two"),
    ])
    replica = Replica(config, store, agent, Transport())
    first = message()
    process(replica, first)
    process(replica, message("event2", text="answer.txt", timestamp="100.000003"))
    assert agent.responded[1][1].session == "sess-one"
    with store.connection:
        store.connection.execute("UPDATE tasks SET updated=updated-7200")
    process(replica, message("event3", text="more", timestamp="100.000005"))
    assert agent.responded[2][1].session is None
    assert store.task(first)["session"] == "sess-two"


def test_zero_timeout_never_resumes(config, store, message):
    config = replace(config, session_timeout=0)
    agent = Agent(results=[AgentResult("Which file?", "waiting", session="a"), AgentResult("done", session="b")])
    replica = Replica(config, store, agent, Transport())
    process(replica, message())
    process(replica, message("event2", text="answer.txt", timestamp="100.000003"))
    assert [context.session for _entry, context in agent.responded] == [None, None]


def test_turn_limit_wraps_up_with_summary_and_new_thread(config, store, message):
    config = replace(config, max_turns=2)
    agent = Agent(results=[
        AgentResult("Which file?", "waiting", session="sess-one"),
        AgentResult("Done, created it.", session="sess-one"),
        AgentResult("Continuing here.", session="sess-one"),
    ])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    process(replica, first)
    assert not transport.announced
    process(replica, message("event2", text="answer.txt", timestamp="100.000003"))
    # The second delivered reply exhausted the budget: stop notice in the thread, summary in the channel.
    assert [result.text for _m, result, _t, _turn in transport.sent][-1].startswith("This thread has reached its turn limit")
    assert transport.sent[-1][0].thread_id == first.thread_id
    assert len(transport.announced) == 1
    announced_message, text, new_task_id = transport.announced[0]
    assert announced_message.channel_id == "CROOM"
    assert text.startswith("Continuing from a thread that reached its turn limit.")
    assert "SUMMARY of" in text and text.endswith("Reply in this thread to continue.")
    assert "<@" not in text
    summarized = agent.summarized[0]
    assert [m.text for m in summarized.messages][-1] == "Done, created it."
    assert len(summarized.messages) == 4
    old = store.task(first)
    assert old["control_state"] == "paused" and old["continuation"] == "501.000009"
    assert "continues in a new thread" in old["pause_reason"]
    fresh = message("event3", text="<@UOWNER> next step?", timestamp="501.000011", thread_id="501.000009")
    seeded = store.task(fresh)
    assert seeded["task_id"] == new_task_id and seeded["turns"] == 0 and seeded["session"] == "sess-one"
    assert seeded["control_state"] == "active"

    # A further mention in the exhausted thread is recorded as paused and does not wrap up again.
    process(replica, message("event4", text="<@UOWNER> more", timestamp="100.000005"))
    assert store.get("event4")["decision"] == "paused"
    assert len(transport.announced) == 1 and len(agent.summarized) == 1

    # The new thread continues with a fresh budget and the inherited session.
    process(replica, fresh)
    assert agent.responded[-1][1].session == "sess-one" and agent.responded[-1][1].turn == 1
    assert transport.sent[-1][0].thread_id == "501.000009" and transport.sent[-1][1].text == "Continuing here."


def test_turn_limit_wrap_up_survives_summary_failure(config, store, message):
    config = replace(config, max_turns=1)
    agent = Agent(results=[AgentResult("done")])
    agent.summary_error = RuntimeError("model down")
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    process(replica, first)
    assert transport.sent[-1][1].text.startswith("This thread has reached its turn limit, so I'm stopping here. I couldn't post a summary")
    assert not transport.announced
    task = store.task(first)
    assert task["continuation"] == "failed" and task["control_state"] == "paused"
    process(replica, message("event2", text="<@UOWNER> again", timestamp="100.000005"))
    assert len(agent.summarized) == 1 and len(transport.sent) == 2


def test_wrap_up_waits_for_open_file_requests(config, store, message):
    config = replace(config, max_turns=1)
    agent = Agent(results=[AgentResult("Approve this file", "waiting")])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    replica.receive(first)
    store.begin(first, "t1", 1)
    with store.connection:
        store.connection.execute(
            "INSERT INTO file_requests(id,event_id,sender,channel,operation,path,content,before_content,created) "
            "VALUES('r1',?,?,?,'write','/tmp/x','new','old',0)", (first.event_id, first.sender_id, first.channel_id))
    store.save_result(first, AgentResult("Approve this file", "waiting"))
    asyncio.run(replica._deliver(first))
    assert len(transport.sent) == 1 and not transport.announced
    assert store.task(first)["continuation"] is None and not getattr(agent, "summarized", [])


def test_finished_discussion_posts_debrief_once_per_finish(config, store, message):
    agent = Agent(results=[
        AgentResult("Which file?", "waiting", finished=True),        # waiting can never finish
        AgentResult("All done, PR #12 merged.", finished=True),
        AgentResult("You're welcome.", finished=True),
    ])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    process(replica, first)
    assert not transport.announced and not getattr(agent, "debriefed", [])
    process(replica, message("event2", text="answer.txt", timestamp="100.000003"))
    assert len(transport.announced) == 1
    _m, text, task_id = transport.announced[0]
    assert text.startswith("Debrief: this discussion is finished.") and "DEBRIEF after 2 turns" in text
    assert "<@" not in text and task_id == store.task(first)["task_id"]
    assert [m.text for m in agent.debriefed[0].messages][-1] == "All done, PR #12 merged."
    assert store.task(first)["debriefed_turn"] == 2 and store.task(first)["control_state"] == "active"
    stored = json.loads(store.get("event2")["result"])
    assert stored["finished"] is True and transport.sent[-1][1].text == "All done, PR #12 merged."
    # A later finished reply in the same thread produces a new debrief because there was a new turn.
    process(replica, message("event3", text="<@UOWNER> thanks!", timestamp="100.000005"))
    assert len(transport.announced) == 2 and store.task(first)["debriefed_turn"] == 3


def test_debrief_failure_is_logged_not_fatal(config, store, message, caplog):
    agent = Agent(results=[AgentResult("done", finished=True)])
    agent.debrief_error = RuntimeError("model down")
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    process(replica, message())
    assert transport.sent[-1][1].text == "done" and not transport.announced
    assert "Debrief for thread" in caplog.text


def test_finished_at_turn_limit_debriefs_instead_of_continuing(config, store, message):
    config = replace(config, max_turns=1)
    agent = Agent(results=[AgentResult("done", finished=True)])
    transport = Transport()
    replica = Replica(config, store, agent, transport)
    first = message()
    process(replica, first)
    assert len(transport.announced) == 1 and transport.announced[0][1].startswith("Debrief:")
    assert not getattr(agent, "summarized", []) and len(transport.sent) == 1
    assert store.task(first)["continuation"] == "debriefed"
