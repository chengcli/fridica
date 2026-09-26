"""A scenario harness: the real Daemon with fake Slack, a scripted parent LLM, and in-memory workers."""

from __future__ import annotations

import asyncio
import json

from fridica.app import Daemon
from fridica.core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from fridica.core.models import FridicaMeta, Message, WorkerResult
from fridica.parent.agent import ParentAgent
from fridica.workers.protocol import Outcome

OWNER, TEAM, ROOM = "UOWNER", "TTEAM", "CROOM"


class FakeSlack:
    def __init__(self, next_ts):
        self.posts: list[dict] = []
        self.uploads: list[dict] = []
        self.failures: list[Exception] = []
        self.next_ts = next_ts
        self.fetched: dict = {}
        self.names: dict = {}

    async def post(self, channel, text, *, thread_ts, meta):
        if self.failures:
            raise self.failures.pop(0)
        ts = self.next_ts()
        self.posts.append({"channel": channel, "text": text, "thread_ts": thread_ts, "meta": meta, "ts": ts})
        return ts

    async def upload(self, channel, thread_ts, data, filename):
        self.uploads.append({"channel": channel, "thread_ts": thread_ts, "data": data, "filename": filename})
        return "F1"

    async def fetch(self, channel, ts, thread):
        return self.fetched.get((channel, ts), [])

    async def recent(self, channel, oldest, threads=()):
        return []

    async def user_name(self, user):
        return self.names.get(user, user)


class ScriptedLLM:
    """Answers parent calls with a function of (call kind, data); records every call."""
    backend = "claude"

    def __init__(self, script):
        self.script = script
        self.calls: list[tuple[str, dict]] = []

    async def call(self, prompt, schema, *, model=""):
        data = json.loads(prompt.split("\n\nData:\n", 1)[1]) if "\n\nData:\n" in prompt else {}
        kind = ("triage" if "decision" in schema["properties"] else "debrief" if "debrief" in schema["properties"]
                else "decide")
        self.calls.append((kind, data))
        response = self.script(kind, data)
        if isinstance(response, Exception):
            raise response
        return response


def action(text="On it.", *, send=True, status="complete", discussion="ongoing", details="", delegate=(), control=(),
           context=None, summary="", decisions=(), kind="result"):
    return {"reply": {"send": send, "text": text, "details": details, "status": status, "discussion": discussion},
            "delegate": list(delegate), "worker_control": list(control),
            "context": {"machine": "", "workspace": "", "repo": "", "branch": "", **(context or {})},
            "summary": summary, "decisions": list(decisions),
            "note": {"kind": kind, "repo": "", "assignee": "", "next_step": "", "blocker": ""}}


def delegation(brief="Investigate.", **values):
    base = {"worker_id": "", "machine": "", "tags": [], "workspace": "", "backend": "", "role": "general",
            "ephemeral": False, "brief": brief, "deliverable": "report"}
    return {**base, **values}


class FakeWorker:
    def __init__(self, spec, harness):
        self.spec = spec
        self.harness = harness
        self._alive = False
        self._busy = False

    @property
    def alive(self):
        return self._alive

    @property
    def busy(self):
        return self._busy

    async def run(self, brief, *, resume, on_approval):
        self._alive = self._busy = True
        try:
            self.harness.jobs_seen.append({"worker": self.spec.worker_id, "machine": self.spec.machine.name,
                                           "workspace": self.spec.workspace.name, "brief": brief, "resume": resume})
            result = self.harness.work(self.spec, brief, resume)
            if isinstance(result, Exception):
                raise result
            return Outcome(result, resume or f"session-{self.spec.worker_id}")
        finally:
            self._busy = False

    async def interrupt(self):
        pass

    async def close(self):
        self._alive = False


class FakeClock:
    def __init__(self):
        self.offset = 0.0

    def now(self):
        return 1_000_000.0 + self.offset

    async def sleep(self, seconds):
        await asyncio.sleep(0)


class Harness:
    def __init__(self, config, store, script, work=None, *, observe_only=False):
        self.counter = 0
        self.clock = FakeClock()
        self.slack = FakeSlack(self.next_ts)
        self.llm = ScriptedLLM(script)
        self.jobs_seen: list[dict] = []
        self.work = work or (lambda spec, brief, resume: WorkerResult("done", f"did {brief[-30:]}", report="Done."))
        self.daemon = Daemon(config, self.slack, store=store, parent=ParentAgent(config, self.llm),
                             factory=lambda spec: FakeWorker(spec, self), observe_only=observe_only, clock=self.clock)

    def next_ts(self) -> str:
        """Message and post timestamps share one increasing sequence, like Slack's."""
        self.counter += 1
        return f"100.{self.counter:06d}"

    @property
    def store(self):
        return self.daemon.store

    def message(self, text, *, thread=None, sender="UALICE", meta=None, ts=None, channel=ROOM):
        ts = ts or self.next_ts()
        message = Message(f"event-{ts}", TEAM, channel, ts, thread, sender, text, meta=meta)
        self.daemon.receive(message)
        return message

    async def settle(self):
        daemon = self.daemon
        for _ in range(100):
            daemon.threads.sweep()
            await daemon.threads.idle()
            daemon.supervisor.schedule()
            running = [item.task for item in daemon.supervisor.running.values()]
            if running:
                await asyncio.gather(*running, return_exceptions=True)
            await daemon.dispatcher.drain()
            await asyncio.sleep(0)
            if (not daemon.store.inbox.pending_sessions() and not daemon.store.jobs.queued()
                    and not daemon.supervisor.running and not daemon.store.outbox.ready(daemon.clock.now())
                    and not daemon.threads.actors):
                later = [item.retry_at for item in daemon.store.outbox.list(states=("pending",))]
                if not later:
                    return
                self.clock.offset += max(later) - self.clock.now()  # jump to a rate-limited retry
        raise AssertionError("the daemon did not settle")

    def texts(self):
        return [post["text"] for post in self.slack.posts]


def peer(owner="UPEER", **values):
    data = {"owner": owner, "session": "", "turn": 1, "status": "complete"}
    data.update(values)
    return FridicaMeta(**data)


__all__ = ["DeliveryAmbiguous", "DeliveryRejected", "Harness", "RateLimited", "action", "delegation", "peer"]
