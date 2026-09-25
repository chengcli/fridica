import asyncio
import stat

import pytest

from fridica.approvals.broker import ApprovalBroker
from fridica.config import editor
from fridica.control.api import serve
from fridica.control.client import ControlClient, ControlError, DaemonUnavailable
from fridica.core.bus import Bus
from fridica.core.clock import Clock
from fridica.core.models import Job, OutboxItem, ThreadKey, WorkerRecord
from fridica.workers.protocol import ApprovalRequest


class FakeSupervisor:
    def __init__(self):
        self.live = {}
        self.calls = []

    async def interrupt(self, worker_id):
        self.calls.append(("interrupt", worker_id))
        return True

    async def stop(self, worker_id):
        self.calls.append(("stop", worker_id))

    def busy_by_machine(self):
        return {"snowy": 1}


class Controls:
    def __init__(self, config, store):
        self.config = config
        self.store = store
        self.bus = Bus()
        self.clock = Clock()
        self.supervisor = FakeSupervisor()
        self.broker = ApprovalBroker(config, store)
        self.started_at = 1.0
        self.observe_only = False
        self.slack_status = "connected"
        self.actions = []

    async def thread_action(self, session_id, action, actor):
        self.actions.append((session_id, action, actor))
        return {"session": session_id, "action": action}

    def update_limits(self, changes):
        self.config = editor.update(self.config.path, {"limits": changes})
        return self.config


@pytest.fixture
def api(config, store, tmp_path):
    controls = Controls(config, store)
    socket = tmp_path / "run" / "control.sock"
    session = store.threads.ensure(ThreadKey("TTEAM", "CROOM", "1.0"), 1.0)
    store.workers.add(WorkerRecord("w1", session.id, "snowy", "exocubed", "codex"), 1.0)
    store.jobs.add(Job("j1", "w1", session.id, "brief"), 1.0)
    return controls, socket, session


def with_server(controls, socket, body):
    async def scenario():
        runner = await serve(controls, socket)
        try:
            return await body(ControlClient(socket))
        finally:
            await runner.cleanup()
    return asyncio.run(scenario())


def test_socket_is_owner_only_and_status_works(api):
    controls, socket, _ = api

    async def body(client):
        mode = stat.S_IMODE(socket.stat().st_mode)
        return mode, await client.request("GET", "/status")

    mode, status = with_server(controls, socket, body)
    assert mode == 0o600
    assert status["owner"] == "UOWNER" and status["queued_jobs"] == 1 and status["slack"] == "connected"


def test_threads_workers_and_actions(api):
    controls, socket, session = api

    async def body(client):
        threads = await client.request("GET", "/threads")
        detail = await client.request("GET", f"/threads/{session.id}")
        action = await client.request("POST", f"/threads/{session.id}/resume", {"actor": "UOWNER"})
        notes = await client.request("POST", f"/threads/{session.id}/notes", {"data": {"goal": "ship"}})
        workers = await client.request("GET", "/workers")
        stopped = await client.request("POST", "/workers/w1/stop")
        with pytest.raises(ControlError) as missing:
            await client.request("GET", "/threads/nope")
        with pytest.raises(ControlError) as bad:
            await client.request("POST", f"/threads/{session.id}/explode")
        return threads, detail, action, notes, workers, stopped, missing.value.status, bad.value.status

    threads, detail, action, notes, workers, stopped, missing, bad = with_server(controls, socket, body)
    assert threads[0]["id"] == session.id and detail["workers"][0]["id"] == "w1" and detail["jobs"][0]["id"] == "j1"
    assert action == {"session": session.id, "action": "resume"} and controls.actions == [(session.id, "resume", "UOWNER")]
    assert notes == {"revision": 1} and workers[0]["process"] == "stopped"
    assert stopped == {"stopped": True} and controls.supervisor.calls == [("stop", "w1")]
    assert (missing, bad) == (404, 404)


def test_approvals_round_trip(api):
    controls, socket, session = api
    worker = controls.store.workers.get("w1")
    job = controls.store.jobs.get("j1")

    async def body(client):
        waiting = asyncio.create_task(controls.broker.request(worker=worker, job=job,
                                                              request=ApprovalRequest("command", "Run make", {"command": "make"})))
        await asyncio.sleep(0.05)
        pending = await client.request("GET", "/approvals")
        decided = await client.request("POST", f"/approvals/{pending[0]['id']}", {"decision": "once"})
        with pytest.raises(ControlError) as again:
            await client.request("POST", f"/approvals/{pending[0]['id']}", {"decision": "deny"})
        return pending, decided, await waiting, again.value.status

    pending, decided, outcome, again = with_server(controls, socket, body)
    assert pending[0]["summary"] == "Run make" and decided == {"decided": "once"} and outcome == "once" and again == 409


def test_outbox_retry_and_machines(api):
    controls, socket, session = api
    store = controls.store
    store.outbox.enqueue(OutboxItem("k1", session.id, "reply", "CROOM", "1.0", "hi", blob=b"x"), 1.0)
    item = store.outbox.get("k1")
    store.outbox.claim(item.id)
    store.outbox.fail(item.id, "ambiguous", "5xx")

    async def body(client):
        problems = await client.request("GET", "/outbox")
        retried = await client.request("POST", f"/outbox/{item.id}/retry")
        machines = await client.request("GET", "/machines")
        return problems, retried, machines

    problems, retried, machines = with_server(controls, socket, body)
    assert problems[0]["state"] == "ambiguous" and problems[0]["has_file"] and "blob" not in problems[0]
    assert retried == {"requeued": True} and store.outbox.get("k1").state == "pending"
    snowy = next(item for item in machines if item["name"] == "snowy")
    assert snowy["busy_jobs"] == 1 and snowy["transport"] == "ssh" and "exocubed" in snowy["workspaces"]


def test_limits_patch_validates_and_rewrites_the_file(api):
    controls, socket, _ = api

    async def body(client):
        changed = await client.request("PATCH", "/config/limits", {"max_wait_replies": 9})
        with pytest.raises(ControlError) as invalid:
            await client.request("PATCH", "/config/limits", {"max_wait_replies": 0})
        with pytest.raises(ControlError) as forbidden:
            await client.request("PATCH", "/config/limits", {"reply_chars": 10})
        return changed, invalid.value, forbidden.value

    changed, invalid, forbidden = with_server(controls, socket, body)
    assert changed["limits"]["max_wait_replies"] == 9 and "max_wait_replies = 9" in controls.config.path.read_text()
    assert invalid.status == 400 and "max_wait_replies" in str(invalid) and forbidden.status == 400
    assert "[owner]" in controls.config.path.read_text()


def test_client_reports_a_missing_daemon(tmp_path):
    with pytest.raises(DaemonUnavailable, match="not running"):
        ControlClient(tmp_path / "none.sock").call("GET", "/status")
