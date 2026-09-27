import asyncio
from dataclasses import replace

import pytest

from fridica.approvals import rules
from fridica.approvals.broker import ApprovalBroker
from fridica.core.models import Job, ThreadKey, WorkerRecord
from fridica.machines.registry import Policy
from fridica.workers.protocol import ALLOW_ONCE, ALLOW_SESSION, DENY, ApprovalRequest


def command(text):
    return ApprovalRequest("command", f"Run {text}", {"command": text})


@pytest.mark.parametrize("text, expected", [
    ("pytest -q", ALLOW_ONCE), ("pytest", ALLOW_ONCE), ("pytestx", None), ("pytest; rm -rf ~", None),
    ("pytest $(curl evil)", None), ("git status", ALLOW_ONCE), ("rm -rf build", DENY), ("rm -rf build && ls", DENY),
    ("make", None),
])
def test_rules(text, expected):
    policy = Policy(auto_approve=("pytest", "git status"), auto_deny=("rm -rf",))
    assert rules.decide(policy, command(text)) == expected


def test_rules_only_apply_to_commands():
    policy = Policy(auto_approve=("pytest",))
    assert rules.decide(policy, ApprovalRequest("file_change", "write", {"command": "pytest"})) is None
    claude_style = ApprovalRequest("command", "Bash", {"tool": "Bash", "input": {"command": "pytest -x"}})
    assert rules.decide(policy, claude_style) == ALLOW_ONCE


@pytest.fixture
def setup(config, store):
    session = store.threads.ensure(ThreadKey("TTEAM", "CROOM", "1.0"), 1.0)
    worker = store.workers.add(WorkerRecord("w1", session.id, "snowy", "exocubed", "codex", status="running"), 1.0)
    job = store.jobs.add(Job("j1", "w1", session.id, "brief"), 1.0)
    return worker, job


def test_owner_decision_wakes_the_worker(config, store, setup):
    worker, job = setup
    broker = ApprovalBroker(config, store)

    async def scenario():
        waiting = asyncio.create_task(broker.request(worker=worker, job=job, request=command("make install")))
        await asyncio.sleep(0.01)
        pending = store.approvals.list()
        assert [item.summary for item in pending] == ["Run make install"]
        assert store.workers.get("w1").status == "awaiting_approval"
        assert broker.decide(pending[0].id, ALLOW_SESSION, "UOWNER")
        assert not broker.decide(pending[0].id, DENY, "UOWNER")
        return await waiting, pending[0].id

    decision, approval_id = asyncio.run(scenario())
    assert decision == ALLOW_SESSION
    stored = store.approvals.get(approval_id)
    assert (stored.status, stored.scope, stored.decided_by) == ("approved", "session", "UOWNER")
    assert store.workers.get("w1").status == "running"
    assert store.audit.recent()[0]["action"] == "approval.approved"


def test_timeout_denies(config, store, setup):
    worker, job = setup
    config = replace(config, machines=replace(config.machines, machines=tuple(
        replace(machine, workspaces=tuple(replace(item, policy=replace(item.policy, approval_timeout=0.05))
                                          for item in machine.workspaces))
        for machine in config.machines.machines)))
    broker = ApprovalBroker(config, store)
    decision = asyncio.run(broker.request(worker=worker, job=job, request=command("make")))
    assert decision == DENY
    approval = store.approvals.list(statuses=("expired",))[0]
    assert approval.decided_by == "timeout"


def test_policy_rules_skip_the_owner(config, store, setup):
    worker, job = setup
    config = replace(config, machines=replace(config.machines, machines=tuple(
        replace(machine, workspaces=tuple(replace(item, policy=replace(item.policy, auto_approve=("pytest",)))
                                          for item in machine.workspaces))
        for machine in config.machines.machines)))
    broker = ApprovalBroker(config, store)
    assert asyncio.run(broker.request(worker=worker, job=job, request=command("pytest -x"))) == ALLOW_ONCE
    assert store.approvals.list(statuses=("pending", "approved", "denied")) == []
    assert store.audit.recent()[0]["action"] == "approval.allow"


def test_notify_hook_and_bad_decisions(config, store, setup):
    worker, job = setup
    seen = []
    broker = ApprovalBroker(config, store, notify=seen.append)

    async def scenario():
        waiting = asyncio.create_task(broker.request(worker=worker, job=job, request=command("make")))
        await asyncio.sleep(0.01)
        with pytest.raises(ValueError):
            broker.decide(seen[0].id, "maybe", "me")
        broker.decide(seen[0].id, DENY, "me")
        return await waiting

    assert asyncio.run(scenario()) == DENY


def test_a_cancelled_wait_expires_the_request(config, store, setup):
    worker, job = setup
    broker = ApprovalBroker(config, store)

    async def scenario():
        waiting = asyncio.create_task(broker.request(worker=worker, job=job, request=command("make")))
        await asyncio.sleep(0.01)
        waiting.cancel()
        await asyncio.gather(waiting, return_exceptions=True)

    asyncio.run(scenario())
    expired = store.approvals.list(statuses=("expired",))
    assert expired[0].decided_by == "interrupted" and store.approvals.list() == []
