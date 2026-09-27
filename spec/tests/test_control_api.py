import asyncio
import pathlib
import shutil
import tempfile
import stat
from dataclasses import replace

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

    def update_parent(self, changes):
        self.config = editor.update(self.config.path, {"parent": changes})
        return self.config

    async def instruct_thread(self, session_id, text, client_id):
        return {"instruction_id": self.store.inbox.add(session_id, "owner_instruction", self.clock.now(),
                                                        ref=client_id, payload={"text": text}), "queued": True}


@pytest.fixture
def api(config, store, tmp_path, request):
    controls = Controls(config, store)
    # Unix socket paths are limited to about 104 bytes, and macOS temporary directories are long.
    short = pathlib.Path(tempfile.mkdtemp(prefix="fr-", dir="/tmp"))
    request.addfinalizer(lambda: shutil.rmtree(short, ignore_errors=True))
    socket = short / "control.sock"
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
    assert snowy["workspace_details"][0]["name"] == "exocubed"
    assert snowy["workspace_details"][0]["path"]
    assert snowy["workspace_details"][0]["approvals"] in ("auto", "on-request", "untrusted", "never")


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


def test_dashboard_config_exposes_safe_settings_and_edits_parent_model(api):
    controls, socket, _ = api

    async def body(client):
        before = await client.request("GET", "/config")
        changed = await client.request("PATCH", "/config/parent", {"backend": "codex", "model": "gpt-5.6-sol", "reasoning_effort": "medium"})
        after = await client.request("GET", "/config")
        with pytest.raises(ControlError) as invalid:
            await client.request("PATCH", "/config/parent", {"reasoning_effort": "extreme"})
        with pytest.raises(ControlError) as forbidden:
            await client.request("PATCH", "/config/parent", {"timeout": 10})
        return before, changed, after, invalid.value.status, forbidden.value.status

    before, changed, after, invalid, forbidden = with_server(controls, socket, body)
    assert before["parent"]["backend"] == "claude"
    assert changed["parent"]["model"] == "gpt-5.6-sol"
    assert changed["parent"]["backend"] == "codex"
    assert after["parent"]["reasoning_effort"] == "medium"
    assert "gpt-5.6-sol" in controls.config.path.read_text()
    assert "app_token_env" not in str(after) and "user_token_env" not in str(after)
    assert (invalid, forbidden) == (400, 400)


def test_dashboard_jobs_lists_active_work_with_thread_links(api):
    controls, socket, session = api
    store = controls.store
    store.jobs.add(Job("j2", "w1", session.id, "finished work"), 2.0)
    store.jobs.finish("j2", "done", 3.0)

    async def body(client):
        return await client.request("GET", "/jobs")

    jobs = with_server(controls, socket, body)
    assert [(job["id"], job["session_id"], job["brief"]) for job in jobs] == [
        ("j1", session.id, "brief")]


def test_attention_threads_include_paused_and_blocked_outside_recent_page(api):
    controls, socket, session = api
    controls.store.threads.save(replace(session, control="paused"), 2.0)
    for index in range(120):
        controls.store.threads.ensure(ThreadKey("TTEAM", "CROOM", f"{index + 10}.0"), index + 10.0)

    async def body(client):
        return await client.request("GET", "/attention/threads")

    items = with_server(controls, socket, body)
    assert [item["id"] for item in items] == [session.id]


def test_owner_instruction_validates_text_and_is_visible_in_thread_detail(api):
    controls, socket, session = api

    async def body(client):
        created = await client.request("POST", f"/threads/{session.id}/instruct",
                                       {"text": "Use the existing worker to check the failing test.", "client_id": "instruction-1"})
        detail = await client.request("GET", f"/threads/{session.id}")
        with pytest.raises(ControlError) as empty:
            await client.request("POST", f"/threads/{session.id}/instruct", {"text": " ", "client_id": "instruction-2"})
        with pytest.raises(ControlError) as missing:
            await client.request("POST", "/threads/missing/instruct", {"text": "check", "client_id": "instruction-3"})
        return created, detail, empty.value.status, missing.value.status

    created, detail, empty, missing = with_server(controls, socket, body)
    assert created["queued"] and detail["instructions"][0]["text"].startswith("Use the existing worker")
    assert (empty, missing) == (400, 404)


def test_client_reports_a_missing_daemon(tmp_path):
    with pytest.raises(DaemonUnavailable, match="not running"):
        ControlClient(tmp_path / "none.sock").call("GET", "/status")
