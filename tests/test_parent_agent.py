import asyncio
from dataclasses import replace

import pytest

from fridica.core.errors import BackendError
from fridica.core.models import StickyContext, ThreadKey, ThreadSession, WorkerRecord
from fridica.parent.actions import Rules, validate
from fridica.parent.agent import ParentAgent, ParentUnavailable
from fridica.parent.prompts import ParentContext
from fridica.parent.schemas import ACTION_SCHEMA


def action(reply=None, delegate=(), control=(), context=None, summary="", decisions=(), note=None):
    return {"reply": {"send": True, "text": "On it.", "details": "", "status": "complete", "discussion": "ongoing", **(reply or {})},
            "delegate": list(delegate), "worker_control": list(control),
            "context": {"machine": "", "workspace": "", "repo": "", "branch": "", **(context or {})},
            "summary": summary, "decisions": list(decisions),
            "note": {"kind": "result", "repo": "", "assignee": "", "next_step": "", "blocker": "", **(note or {})}}


def delegation(**values):
    base = {"worker_id": "", "machine": "", "tags": [], "workspace": "", "backend": "", "role": "general",
            "ephemeral": False, "brief": "Investigate the CUDA init failure.", "deliverable": "report"}
    return {**base, **values}


SESSION = ThreadSession("TTEAM:CROOM:1.0", ThreadKey("TTEAM", "CROOM", "1.0"))


@pytest.fixture
def rules(config):
    def make(**changes):
        values = {"registry": config.machines, "session": SESSION, "workers": (), "may_delegate": True,
                  "max_delegations": 3, "max_workers": 2, "busy": {}}
        values.update(changes)
        return Rules(**values)
    return make


def test_schema_is_strict():
    def check(node):
        if node.get("type") == "object":
            assert node["additionalProperties"] is False and set(node["required"]) == set(node["properties"])
            for child in node["properties"].values():
                check(child)
        if node.get("type") == "array":
            check(node["items"])
    check(ACTION_SCHEMA)


def test_reply_only(rules):
    result, errors = validate(action(reply={"discussion": "finished"}, decisions=["use gcc 14"]), rules())
    assert errors == [] and result.reply.finished and result.delegations == () and result.decisions == ("use gcc 14",)
    quiet, _ = validate(action(reply={"send": True, "text": "  "}), rules())
    assert not quiet.reply.send
    waiting, _ = validate(action(reply={"status": "waiting", "discussion": "finished"}), rules())
    assert waiting.reply.discussion == "ongoing"


def test_fan_out_to_two_machines_and_follow_up_to_an_existing_worker(rules):
    raw = action(delegate=[delegation(machine="snowy", workspace="canoe"), delegation(tags=["gcc"], workspace="canoe")])
    result, errors = validate(raw, rules())
    assert errors == []
    assert [(item.placement.machine.name, item.placement.workspace.name, item.placement.backend) for item in result.delegations] == [
        ("snowy", "canoe", "codex"), ("dart9", "canoe", "codex")]
    worker = WorkerRecord("w1", SESSION.id, "snowy", "canoe", "codex", status="idle")
    result, errors = validate(action(delegate=[delegation(worker_id="w1", brief="Fix it and rerun.")]), rules(workers=(worker,)))
    assert errors == [] and result.delegations[0].worker_id == "w1" and result.delegations[0].placement is None


def test_sticky_context_places_follow_ups(rules):
    session = replace(SESSION, context=StickyContext(machine="snowy", workspace="exocubed"))
    result, errors = validate(action(delegate=[delegation()]), rules(session=session))
    assert errors == [] and result.delegations[0].placement.workspace.name == "exocubed"


def test_problems_are_reported_for_repair(rules):
    worker = WorkerRecord("w1", SESSION.id, "snowy", "canoe", "codex", status="stopped")
    raw = action(delegate=[delegation(machine="mars"), delegation(worker_id="w9"), delegation(worker_id="w1"),
                           delegation(brief=""), delegation(workspace="canoe")])
    result, errors = validate(raw, rules(workers=(worker,), max_delegations=5))
    assert result.delegations == ()
    assert any("unknown machine 'mars'" in error for error in errors)
    assert any("does not belong" in error for error in errors) and any("stopped" in error for error in errors)
    assert any("brief is empty" in error for error in errors) and any("several machines" in error for error in errors)


def test_limits_and_channel_policy(rules):
    busy = tuple(WorkerRecord(f"w{index}", SESSION.id, "snowy", "canoe", "codex") for index in range(2))
    result, errors = validate(action(delegate=[delegation(machine="dart9")]), rules(workers=busy))
    assert result.delegations == () and "limit 2" in errors[0]
    result, errors = validate(action(delegate=[delegation(machine="dart9", ephemeral=True)]), rules(workers=busy))
    assert errors == [] and result.delegations[0].ephemeral
    result, errors = validate(action(delegate=[delegation(machine="dart9")]), rules(may_delegate=False))
    assert result.delegations == () and "not allowed" in errors[0]
    result, errors = validate(action(delegate=[delegation(machine="dart9")] * 4), rules(max_workers=5))
    assert len(result.delegations) == 3 and "at most 3" in errors[0]


def test_context_and_controls_are_validated(rules):
    worker = WorkerRecord("w1", SESSION.id, "snowy", "canoe", "codex")
    raw = action(context={"machine": "snowy", "workspace": "canoe", "branch": "fix/cuda"},
                 control=[{"worker_id": "w1", "op": "interrupt"}, {"worker_id": "nope", "op": "stop"}])
    result, _ = validate(raw, rules(workers=(worker,)))
    assert result.context == {"machine": "snowy", "workspace": "canoe", "branch": "fix/cuda"}
    assert [(item.worker_id, item.op) for item in result.controls] == [("w1", "interrupt")]
    result, _ = validate(action(context={"machine": "mars", "workspace": "nowhere"}), rules())
    assert result.context == {}


class ScriptedLLM:
    backend = "claude"

    def __init__(self, *responses):
        self.responses = list(responses)
        self.prompts = []

    async def call(self, prompt, schema, *, model=""):
        self.prompts.append((prompt, model))
        response = self.responses.pop(0)
        if isinstance(response, Exception):
            raise response
        return response


def context():
    return ParentContext("UOWNER", "profile", SESSION, {"kind": "message", "message": {"text": "fix it"}},
                         history=({"sender": "UALICE", "text": "fix it"},))


def test_decide_repairs_once_then_drops(config, rules):
    llm = ScriptedLLM(action(delegate=[delegation(machine="mars")]), action(delegate=[delegation(machine="snowy", workspace="canoe")]))
    agent = ParentAgent(config, llm)
    ledger = []
    result = asyncio.run(agent.decide(context(), rules(), ledger=ledger))
    assert result.delegations[0].placement.machine.name == "snowy"
    assert "repair" in llm.prompts[1][0] and "unknown machine" in llm.prompts[1][0]
    assert [call["call"] for call in ledger] == ["decide", "repair"] and all("latency_ms" in call for call in ledger)
    llm = ScriptedLLM(action(delegate=[delegation(machine="mars")]), action(delegate=[delegation(machine="mars")]))
    result = asyncio.run(ParentAgent(config, llm).decide(context(), rules()))
    assert result.delegations == () and result.reply.text == "On it."


def test_failures(config, rules):
    with pytest.raises(ParentUnavailable):
        asyncio.run(ParentAgent(config, ScriptedLLM(BackendError("down"))).decide(context(), rules()))
    triage = ParentAgent(replace(config, parent=replace(config.parent, triage_model="small")), ScriptedLLM(BackendError("down")))
    assert asyncio.run(triage.triage(context())) == "observe"
    llm = ScriptedLLM({"decision": "respond"})
    assert asyncio.run(ParentAgent(replace(config, parent=replace(config.parent, triage_model="small")), llm).triage(context())) == "respond"
    assert llm.prompts[0][1] == "small" and "Participation" not in llm.prompts[0][0]


def test_debrief_and_worker_instructions(config):
    agent = ParentAgent(config, ScriptedLLM({"debrief": "x" * 3000}, {"debrief": ""}))
    assert len(asyncio.run(agent.debrief(context()))) == 2500
    with pytest.raises(BackendError, match="empty"):
        asyncio.run(agent.debrief(context()))
    text = agent.worker_instructions(machine={"name": "snowy"}, workspace="canoe")
    assert "report field" in text and '"workspace": "canoe"' in text and "## Repo rules" in text


def test_decide_prompt_carries_data_not_instructions(config, rules):
    llm = ScriptedLLM(action())
    asyncio.run(ParentAgent(config, llm).decide(context(), rules()))
    prompt = llm.prompts[0][0]
    rules_text, data = prompt.split("\n\nData:\n", 1)
    assert "untrusted" in rules_text and '"trigger": {"kind": "message"' in data and "Delegate work" in rules_text
