"""The daemon's control API: JSON over HTTP on a Unix socket only the owner can open.

The CLI and the dashboard never write the database; every change goes through the
daemon, which applies it in its own transactions and wakes the right component.
"""

from __future__ import annotations

import logging
import os
import stat
from pathlib import Path
from typing import Any, Protocol

from aiohttp import web

from ..config.schema import Config
from ..core.errors import ConfigError
from ..store import Store
from . import views

logger = logging.getLogger(__name__)
THREAD_ACTIONS = ("resume", "pause", "close", "archive", "restore", "clean")
LIMIT_KEYS = ("max_wait_replies", "max_no_progress", "max_delegations_per_turn",
              "max_workers_per_thread", "max_jobs", "job_timeout", "worker_idle")


class Controls(Protocol):
    """What the API needs from the daemon."""
    config: Config
    store: Store
    supervisor: Any
    broker: Any
    bus: Any
    clock: Any
    started_at: float
    observe_only: bool
    slack_status: str

    async def thread_action(self, session_id: str, action: str, actor: str) -> dict: ...

    def update_limits(self, changes: dict) -> Config: ...


def create_app(controls: Controls) -> web.Application:
    app = web.Application(middlewares=[_errors])
    routes = web.RouteTableDef()

    @routes.get("/status")
    async def status(request):
        store = controls.store
        return web.json_response({
            "owner": controls.config.owner.slack_user, "started_at": controls.started_at,
            "observe_only": controls.observe_only, "slack": controls.slack_status,
            "pending_approvals": len(store.approvals.list()),
            "running_jobs": len(store.jobs.running()), "queued_jobs": len(store.jobs.queued()),
            "problem_posts": len(store.outbox.list()),
            "config": {"path": str(controls.config.path), "fingerprint": controls.config.fingerprint},
        })

    @routes.get("/threads")
    async def threads(request):
        control = tuple(filter(None, request.query.get("control", "").split(","))) or None
        limit = int(request.query.get("limit", 100))
        return web.json_response([views.session(item) for item in controls.store.threads.list(control=control, limit=limit)])

    @routes.get("/threads/{id}")
    async def thread(request):
        store = controls.store
        item = store.threads.get(request.match_info["id"])
        if item is None:
            raise web.HTTPNotFound(text="no such thread")
        revision, notes = store.notes.current(item.id)
        workers = store.workers.for_session(item.id)
        return web.json_response({
            "session": views.session(item),
            "messages": [views.message(message) for message in store.messages.thread(item.key, limit=200)],
            "workers": [_worker(controls, worker) for worker in workers],
            "jobs": [views.job(job) for worker in workers for job in store.jobs.for_worker(worker.id)],
            "outbox": [views.outbox(post) for post in store.outbox.for_session(item.id)],
            "notes": {"revision": revision, "data": notes},
        })

    @routes.post("/threads/{id}/notes")
    async def notes(request):
        body = await _body(request)
        if not isinstance(body.get("data"), dict):
            raise web.HTTPBadRequest(text="data must be an object")
        session_id = request.match_info["id"]
        if controls.store.threads.get(session_id) is None:
            raise web.HTTPNotFound(text="no such thread")
        revision = controls.store.notes.write(session_id, body["data"], body.get("actor", "owner"),
                                              controls.clock.now(), source="control",
                                              expected=body.get("expected"))
        return web.json_response({"revision": revision})

    @routes.post("/threads/{id}/{action}")
    async def thread_action(request):
        action = request.match_info["action"]
        if action not in THREAD_ACTIONS:
            raise web.HTTPNotFound(text="unknown thread action")
        body = await _body(request)
        result = await controls.thread_action(request.match_info["id"], action, body.get("actor", "owner"))
        return web.json_response(result)

    @routes.get("/workers")
    async def workers(request):
        statuses = tuple(filter(None, request.query.get("status", "").split(","))) or None
        return web.json_response([_worker(controls, item) for item in controls.store.workers.all(statuses=statuses)])

    @routes.post("/workers/{id}/{op}")
    async def worker_op(request):
        worker_id, op = request.match_info["id"], request.match_info["op"]
        if controls.store.workers.get(worker_id) is None:
            raise web.HTTPNotFound(text="no such worker")
        if op == "interrupt":
            return web.json_response({"interrupted": await controls.supervisor.interrupt(worker_id)})
        if op == "stop":
            await controls.supervisor.stop(worker_id)
            return web.json_response({"stopped": True})
        raise web.HTTPNotFound(text="unknown worker operation")

    @routes.get("/approvals")
    async def approvals(request):
        statuses = tuple(filter(None, request.query.get("status", "pending").split(",")))
        return web.json_response([views.approval(item) for item in controls.store.approvals.list(statuses=statuses)])

    @routes.post("/approvals/{id}")
    async def decide(request):
        body = await _body(request)
        decision = body.get("decision")
        if decision not in ("once", "session", "deny"):
            raise web.HTTPBadRequest(text="decision must be once, session, or deny")
        if controls.store.approvals.get(request.match_info["id"]) is None:
            raise web.HTTPNotFound(text="no such approval")
        changed = controls.broker.decide(request.match_info["id"], decision, body.get("actor", "owner"))
        if not changed:
            raise web.HTTPConflict(text="this request was already decided")
        return web.json_response({"decided": decision})

    @routes.get("/machines")
    async def machines(request):
        busy = controls.supervisor.busy_by_machine()
        live = {}
        for worker in controls.supervisor.live.values():
            if worker.alive:
                live[worker.spec.machine.name] = live.get(worker.spec.machine.name, 0) + 1
        payload = []
        for machine in controls.config.machines.machines:
            data = machine.payload(busy.get(machine.name, 0))
            data.update(transport=machine.transport, host=machine.host, live_workers=live.get(machine.name, 0),
                        max_workers=machine.max_workers)
            payload.append(data)
        return web.json_response(payload)

    @routes.get("/outbox")
    async def outbox(request):
        states = tuple(filter(None, request.query.get("state", "failed,ambiguous").split(",")))
        return web.json_response([views.outbox(item) for item in controls.store.outbox.list(states=states)])

    @routes.post("/outbox/{id}/retry")
    async def retry(request):
        if not controls.store.outbox.requeue(int(request.match_info["id"])):
            raise web.HTTPConflict(text="only failed or ambiguous posts can be retried")
        controls.bus.outbox.ring()
        return web.json_response({"requeued": True})

    @routes.get("/activity")
    async def activity(request):
        return web.json_response(controls.store.audit.recent(limit=int(request.query.get("limit", 200))))

    @routes.patch("/config/limits")
    async def limits(request):
        body = await _body(request)
        unknown = set(body) - set(LIMIT_KEYS)
        if unknown:
            raise web.HTTPBadRequest(text=f"not editable here: {', '.join(sorted(unknown))}")
        try:
            config = controls.update_limits(body)
        except ConfigError as error:
            raise web.HTTPBadRequest(text=str(error)) from None
        return web.json_response({"limits": {key: getattr(config.limits, key) for key in LIMIT_KEYS}})

    app.add_routes(routes)
    return app


def _worker(controls: Controls, record) -> dict:
    live = controls.supervisor.live.get(record.id)
    return views.worker(record, live=bool(live and live.alive), busy=bool(live and live.busy))


async def _body(request: web.Request) -> dict:
    if not request.can_read_body:
        return {}
    try:
        body = await request.json()
    except ValueError:
        raise web.HTTPBadRequest(text="body must be JSON") from None
    if not isinstance(body, dict):
        raise web.HTTPBadRequest(text="body must be a JSON object")
    return body


@web.middleware
async def _errors(request, handler):
    try:
        return await handler(request)
    except web.HTTPException:
        raise
    except ValueError as error:
        raise web.HTTPBadRequest(text=str(error)) from None
    except Exception:
        logger.exception("control API request %s %s failed", request.method, request.path)
        raise web.HTTPInternalServerError(text="internal error; see the daemon log") from None


async def serve(controls: Controls, path: Path) -> web.AppRunner:
    """Listen on ``path`` with owner-only permissions; returns the runner to clean up."""
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    status = os.lstat(path.parent)
    if not stat.S_ISDIR(status.st_mode) or status.st_uid != os.getuid() or status.st_mode & 0o022:
        raise RuntimeError(f"{path.parent} must be a directory owned by this user and not writable by others")
    path.unlink(missing_ok=True)
    runner = web.AppRunner(create_app(controls), access_log=None)
    await runner.setup()
    previous = os.umask(0o177)
    try:
        site = web.UnixSite(runner, str(path))
        await site.start()
    finally:
        os.umask(previous)
    os.chmod(path, 0o600)
    return runner
