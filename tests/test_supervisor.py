import asyncio
from dataclasses import replace

import pytest

from fridica.core.bus import Bus
from fridica.core.errors import BackendError
from fridica.core.models import ArtifactRef, Job, ThreadKey, WorkerRecord, WorkerResult
from fridica.workers.protocol import ALLOW_ONCE, ApprovalRequest, Outcome
from fridica.workers.supervisor import Supervisor, frame


class FakeWorker:
    """Runs until the test releases it; can ask for approval or fail."""

    def __init__(self, spec, script):
        self.spec = spec
        self.script = script
        self.started = asyncio.Event()
        self.release = asyncio.Event()
        self.closed = False
        self.interrupted = False
        self._busy = False
        self.calls = []

    @property
    def alive(self):
        return not self.closed

    @property
    def busy(self):
        return self._busy

    async def run(self, brief, *, resume, on_approval):
        self._busy = True
        self.closed = False
        self.calls.append((brief, resume))
        try:
            self.started.set()
            action = self.script.get(self.spec.worker_id, {})
            if action.get("approve"):
                decision = await on_approval(ApprovalRequest("command", "run make"))
                action = {**action, "summary": f"approval {decision}"}
            if not action.get("instant"):
                await self.release.wait()
            if self.interrupted:
                raise BackendError("the job was interrupted")
            if action.get("fail"):
                raise BackendError("ssh: connect refused")
            artifacts = tuple(ArtifactRef(path, "png") for path in action.get("artifacts", ()))
            return Outcome(WorkerResult("done", action.get("summary", "ok"), artifacts=artifacts, report="r"),
                           f"session-{self.spec.worker_id}")
        finally:
            self._busy = False

    async def interrupt(self):
        self.interrupted = True
        self.release.set()

    async def close(self):
        self.closed = True


class Gate:
    def __init__(self):
        self.requests = []

    async def request(self, *, worker, job, request):
        self.requests.append((worker.id, job.id, request.summary))
        return ALLOW_ONCE


@pytest.fixture
def harness(config, store):
    config = replace(config, limits=replace(config.limits, max_jobs=3))
    bus = Bus()
    rung = []
    bus.on_thread(rung.append)
    script = {}
    fakes = {}

    def factory(spec):
        fakes[spec.worker_id] = FakeWorker(spec, script)
        return fakes[spec.worker_id]

    gate = Gate()
    supervisor = Supervisor(config, store, bus, instructions=lambda record: f"rules for {record.role}",
                            approvals=gate, factory=factory)
    session = store.threads.ensure(ThreadKey("TTEAM", "CROOM", "1.0"), 1.0)

    def add(worker_id, machine="snowy", workspace="exocubed", backend="codex", jobs=1, **changes):
        store.workers.add(WorkerRecord(worker_id, session.id, machine, workspace, backend, **changes), 1.0)
        for index in range(jobs):
            store.jobs.add(Job(f"{worker_id}-j{index}", worker_id, session.id, f"brief {index}", join_group="g"), 1.0)

    return type("H", (), {"supervisor": supervisor, "store": store, "script": script, "fakes": fakes, "add": add,
                          "rung": rung, "gate": gate, "session": session, "config": config})


async def settle():
    for _ in range(20):
        await asyncio.sleep(0)


def test_limits_one_job_per_worker_per_machine_and_global(harness):
    harness.add("a", jobs=2)
    harness.add("b")
    harness.add("c")
    harness.add("d", machine="dart9", workspace="canoe")
    harness.add("e", machine="dart9", workspace="canoe")

    async def scenario():
        started = harness.supervisor.schedule()
        await settle()
        return started

    started = asyncio.run(scenario())
    # snowy allows 2 jobs, dart9 1, globally 3; worker a runs only its first job.
    assert started == ["a-j0", "b-j0", "d-j0"]


def test_finished_job_records_result_and_notifies_the_thread(harness):
    harness.add("a")

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        harness.fakes["a"].release.set()
        await settle()

    asyncio.run(scenario())
    store = harness.store
    assert store.jobs.get("a-j0").status == "done"
    worker = store.workers.get("a")
    assert worker.status == "idle" and worker.backend_session_id == "session-a" and worker.summary == "ok"
    assert [item.kind for item in store.inbox.pending(harness.session.id)] == ["worker_result"]
    assert harness.rung == [harness.session.id]
    brief, resume = harness.fakes["a"].calls[0]
    assert "worker a on snowy, workspace exocubed" in brief and brief.endswith("brief 0") and resume == ""


def test_next_job_resumes_the_backend_session_unless_stale(harness):
    harness.add("a", jobs=2)
    harness.script["a"] = {"instant": True}

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        harness.supervisor.schedule()
        await settle()

    asyncio.run(scenario())
    assert [call[1] for call in harness.fakes["a"].calls] == ["", "session-a"]


def test_failures_are_recorded_not_raised(harness):
    harness.add("a")
    harness.script["a"] = {"instant": True, "fail": True}

    async def scenario():
        harness.supervisor.schedule()
        await settle()

    asyncio.run(scenario())
    job = harness.store.jobs.get("a-j0")
    assert job.status == "failed" and "connect refused" in job.error
    assert harness.store.workers.get("a").status == "idle"
    assert harness.store.inbox.pending(harness.session.id)[0].ref == "a-j0"


def test_ephemeral_workers_retire_after_their_job(harness):
    harness.add("a", ephemeral=True)
    harness.script["a"] = {"instant": True}

    async def scenario():
        harness.supervisor.schedule()
        await settle()

    asyncio.run(scenario())
    assert harness.store.workers.get("a").status == "stopped" and harness.fakes["a"].closed
    assert "a" not in harness.supervisor.live


def test_approvals_are_routed_through_the_gate(harness):
    harness.add("a")
    harness.script["a"] = {"instant": True, "approve": True}

    async def scenario():
        harness.supervisor.schedule()
        await settle()

    asyncio.run(scenario())
    assert harness.gate.requests == [("a", "a-j0", "run make")]
    assert harness.store.jobs.get("a-j0").result.summary == "approval once"


def test_interrupt_and_stop(harness):
    harness.add("a", jobs=2)
    harness.add("b")

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        assert await harness.supervisor.interrupt("b")
        await settle()
        await harness.supervisor.stop("a")
        await settle()

    asyncio.run(scenario())
    store = harness.store
    assert store.jobs.get("b-j0").status == "interrupted" and store.workers.get("b").status == "idle"
    assert store.jobs.get("a-j0").status == "interrupted" and store.jobs.get("a-j1").status == "cancelled"
    assert store.workers.get("a").status == "stopped"
    refs = sorted(item.ref for item in store.inbox.pending(harness.session.id))
    assert refs == ["a-j0", "a-j1", "b-j0"]


def test_idle_workers_are_evicted_to_respect_max_workers(harness):
    config = harness.config
    snowy = config.machines["snowy"]
    harness.supervisor.config = replace(config, machines=replace(
        config.machines, machines=tuple(replace(item, max_workers=2, max_jobs=2) if item.name == "snowy" else item
                                        for item in config.machines.machines)))
    for name in ("a", "b", "c"):
        harness.add(name)
        harness.script[name] = {"instant": True}
    assert snowy.name == "snowy"

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        harness.supervisor.schedule()
        await settle()

    asyncio.run(scenario())
    assert all(harness.store.jobs.get(f"{name}-j0").status == "done" for name in "abc")
    assert sum(1 for worker in harness.supervisor.live.values() if worker.alive) <= 2


def test_artifacts_are_read_from_the_workspace(harness, workspace):
    (workspace / "plot.png").write_bytes(b"\x89PNG\r\n\x1a\nDATA")
    harness.add("a", machine="local", workspace="project")
    harness.script["a"] = {"instant": True, "artifacts": [str(workspace / "plot.png"), "/etc/passwd.png"]}

    async def scenario():
        harness.supervisor.schedule()
        for _ in range(50):
            await asyncio.sleep(0.01)
            if harness.store.jobs.get("a-j0").status == "done":
                break

    asyncio.run(scenario())
    artifacts = harness.store.artifacts.for_job("a-j0")
    assert [(item["status"], item["size"]) for item in artifacts] == [("ready", 12), ("rejected", 0)]
    assert artifacts[0]["blob"].endswith(b"DATA")


def test_unconfigured_machine_cancels_the_job(harness):
    harness.add("a", machine="mars", workspace="x")
    harness.supervisor.schedule()
    assert harness.store.jobs.get("a-j0").status == "cancelled"


def test_frame_names_the_worker_and_role():
    record = WorkerRecord("w9", "s", "snowy", "exocubed", "codex", role="reviewer")
    text = frame(Job("j", "w9", "s", "Review the diff", deliverable="markdown"), record)
    assert "worker w9 on snowy" in text and "reviewer" in text and "markdown" in text and text.endswith("Review the diff")


def test_one_pass_counts_workers_it_is_starting_and_never_evicts_them(harness):
    config = harness.config
    harness.supervisor.config = replace(config, limits=replace(config.limits, max_jobs=5), machines=replace(
        config.machines, machines=tuple(replace(item, max_workers=2, max_jobs=3) if item.name == "snowy" else item
                                        for item in config.machines.machines)))
    harness.add("x", jobs=0)
    harness.add("y", jobs=0)
    harness.add("x2", jobs=0)
    harness.store.jobs.add(Job("x-follow", "x", harness.session.id, "follow up"), 2.0)
    harness.add("w1")

    async def scenario():
        # x and y have warm idle processes; x gets a follow-up and w1 is new.
        for name in ("x", "y"):
            worker = harness.supervisor._worker(harness.store.workers.get(name))
            worker.closed = False
        original_x = harness.supervisor.live["x"]
        started = harness.supervisor.schedule()
        await settle()
        return started, original_x

    started, original_x = asyncio.run(scenario())
    assert sorted(started) == ["w1-j0", "x-follow"]
    assert harness.supervisor.live["x"] is original_x and not original_x.closed
    assert harness.fakes["y"].closed and "y" not in harness.supervisor.live
    engaged = [key for key, worker in harness.supervisor.live.items() if worker.spec.machine.name == "snowy"]
    assert sorted(engaged) == ["w1", "x"]


def test_a_queued_job_does_not_revive_a_stopped_worker(harness):
    harness.add("a", status="stopped")
    harness.supervisor.schedule()
    assert harness.store.jobs.get("a-j0").status == "cancelled"
    assert harness.store.workers.get("a").status == "stopped"


def test_an_interrupted_job_that_returns_normally_is_recorded_as_interrupted(harness):
    harness.add("a")

    class Graceful(FakeWorker):
        async def interrupt(self):
            self.release.set()  # the backend ends the turn cleanly instead of raising

    harness.supervisor.factory = lambda spec: harness.fakes.setdefault(spec.worker_id, Graceful(spec, harness.script))

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        await harness.supervisor.interrupt("a")
        await settle()

    asyncio.run(scenario())
    assert harness.store.jobs.get("a-j0").status == "interrupted"


def test_stop_that_times_out_finishes_the_job_once(harness, monkeypatch):
    harness.add("a")

    class Stubborn(FakeWorker):
        async def interrupt(self):
            pass

    harness.supervisor.factory = lambda spec: harness.fakes.setdefault(spec.worker_id, Stubborn(spec, harness.script))
    real_wait = asyncio.wait

    async def quick_wait(tasks, timeout=None, **options):
        return await real_wait(tasks, timeout=0.05 if timeout else None, **options)

    monkeypatch.setattr("fridica.workers.supervisor.asyncio.wait", quick_wait)

    async def scenario():
        harness.supervisor.schedule()
        await settle()
        await harness.supervisor.stop("a")
        await settle()

    asyncio.run(scenario())
    assert harness.store.jobs.get("a-j0").status == "cancelled"
    assert [item.ref for item in harness.store.inbox.pending(harness.session.id)] == ["a-j0"]
    assert harness.store.workers.get("a").status == "stopped"
