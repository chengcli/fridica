"""The worker supervisor: live worker processes, the job scheduler, and concurrency limits.

Jobs are rows in SQLite. The scheduler starts queued jobs while respecting one job
per worker, ``max_jobs`` per machine and globally, and ``max_workers`` live
processes per machine (evicting idle ones first). A finished job writes its result,
its artifacts, and a ``worker_result`` inbox row for its thread in one transaction;
workers never talk to the parent directly.
"""

from __future__ import annotations

import asyncio
from collections import Counter
from collections.abc import Callable
from dataclasses import dataclass, replace
import logging
import uuid

from ..config.schema import Config
from ..core.bus import Bus
from ..core.clock import Clock
from ..core.models import Job, WorkerRecord
from ..exec.transport import make_transport
from ..store import Store
from .artifacts import collect
from .protocol import DENY, ApprovalRequest, Worker, WorkerFactory, WorkerSpec

logger = logging.getLogger(__name__)
SCHEDULE_INTERVAL = 5.0


def default_factory(spec: WorkerSpec) -> Worker:
    from .claude import ClaudeWorker
    from .codex import CodexWorker
    return {"claude": ClaudeWorker, "codex": CodexWorker}[spec.backend](spec)


class ApprovalGate:
    """What the supervisor needs from the approval broker."""

    async def request(self, *, worker: WorkerRecord, job: Job, request: ApprovalRequest) -> str:
        return DENY


@dataclass
class Running:
    job_id: str
    worker_id: str
    task: asyncio.Task
    resume: str = ""
    stopping: bool = False
    interrupted: bool = False
    finished: bool = False


class Supervisor:
    def __init__(self, config: Config, store: Store, bus: Bus, *, instructions: Callable[[WorkerRecord], str],
                 approvals: ApprovalGate | None = None, factory: WorkerFactory = default_factory,
                 clock: Clock | None = None):
        self.config = config
        self.store = store
        self.bus = bus
        self.instructions = instructions
        self.approvals = approvals or ApprovalGate()
        self.factory = factory
        self.clock = clock or Clock()
        self.live: dict[str, Worker] = {}
        self.running: dict[str, Running] = {}
        self._closing: set[asyncio.Task] = set()

    # ----- scheduling -----

    async def run(self) -> None:
        while True:
            self.schedule()
            await self.bus.jobs.wait(SCHEDULE_INTERVAL)

    def schedule(self) -> list[str]:
        """Start every queued job the limits allow; return the ids started."""
        now = self.clock.now()
        workers = {record.id: record for record in self.store.workers.all()}
        per_machine = Counter(workers[job.worker_id].machine for job in self.store.jobs.running()
                              if job.worker_id in workers)
        busy_workers = {job.worker_id for job in self.store.jobs.running()}
        # Each running job occupies its worker's slot: its own GPUs and, with subfolders, its own directory.
        occupied = {(workers[job.worker_id].machine, workers[job.worker_id].slot) for job in self.store.jobs.running()
                    if job.worker_id in workers}
        assigned = Counter((record.machine, record.slot) for record in workers.values() if record.status != "stopped")
        total = sum(per_machine.values())
        started = []
        queued = self.store.jobs.queued()
        waiting = {job.worker_id for job in queued}
        for job in queued:
            if total >= self.config.limits.max_jobs:
                break
            record = workers.get(job.worker_id)
            machine = self.config.machines.get(record.machine) if record else None
            if record is None or record.status == "stopped" or machine is None:
                stopped = record is not None and record.status == "stopped"
                self._finish(job, record, "cancelled", error="worker stopped" if stopped else "machine no longer configured",
                             stop=stopped)
                continue
            if job.worker_id in busy_workers or per_machine[machine.name] >= machine.max_jobs:
                continue
            slot = record.slot if 1 <= record.slot <= machine.max_jobs else 0
            if slot and (machine.name, slot) in occupied:
                continue  # its slot is busy with another worker's job; it waits rather than moving
            if not slot:
                free = [item for item in range(1, machine.max_jobs + 1) if (machine.name, item) not in occupied]
                if not free:
                    continue
                slot = min(free, key=lambda item: (assigned[(machine.name, item)], item))
            if not self._make_room(machine.name, machine.max_workers, record.id, keep=waiting):
                continue
            if slot != record.slot:
                self.store.workers.set_slot(record.id, slot)
                assigned[(machine.name, slot)] += 1
                record = replace(record, slot=slot)
                workers[record.id] = record
            try:
                self._worker(record)
            except Exception as error:
                self._finish(job, record, "failed", error=str(error)[:2000])
                continue
            with self.store.transaction():
                if not self.store.jobs.start(job.id, now):
                    continue
                self.store.workers.set_status(record.id, "running", now)
            busy_workers.add(record.id)
            occupied.add((machine.name, slot))
            per_machine[machine.name] += 1
            total += 1
            # A thread quiet for longer than session_timeout starts its worker fresh.
            stale = record.updated and now - record.updated > self.config.limits.session_timeout
            resume = "" if stale else record.backend_session_id
            task = asyncio.create_task(self._run(job.id), name=f"job-{job.id}")
            self.running[job.id] = Running(job.id, record.id, task, resume)
            started.append(job.id)
        return started

    def _make_room(self, machine: str, limit: int, worker_id: str, *, keep: set[str] = frozenset()) -> bool:
        """Whether ``worker_id`` may have a process on ``machine``; evicts an idle one when needed.

        A worker with a job starting or running counts as occupying a slot even before
        its process is up, and is never chosen for eviction; neither is an idle worker
        that has queued work of its own (``keep``), whose warm session would be lost.
        """
        engaged = {item.worker_id for item in self.running.values()}
        occupied = [key for key, worker in self.live.items()
                    if worker.spec.machine.name == machine and (worker.alive or key in engaged)]
        if worker_id in occupied:
            return True
        if len(occupied) < limit:
            return True
        idle = [key for key in occupied if key not in engaged and key not in keep and not self.live[key].busy]
        if not idle:
            return False
        logger.info("closing idle worker %s to make room on %s", idle[0], machine)
        self._close_later(self.live.pop(idle[0]))
        return True

    def _close_later(self, worker: Worker) -> None:
        task = asyncio.create_task(worker.close())
        self._closing.add(task)
        task.add_done_callback(self._closing.discard)

    # ----- running a job -----

    def spec(self, record: WorkerRecord) -> WorkerSpec:
        """How to start this worker: its machine limited to its slot's GPUs, and its slot's subfolder."""
        machine = self.config.machines[record.machine]
        workspace = machine.workspace(record.workspace)
        if workspace is None:
            raise ValueError(f"workspace {record.workspace} is no longer configured on {machine.name}")
        return WorkerSpec(worker_id=record.id, machine=machine.for_slot(record.slot),
                          workspace=workspace.for_slot(record.slot), backend=record.backend,
                          instructions=self.instructions(record), model="", reasoning_effort="",
                          job_timeout=self.config.limits.job_timeout, idle_timeout=self.config.limits.worker_idle,
                          excluded_env=self.config.secret_env(),
                          slot=record.slot)

    def _worker(self, record: WorkerRecord) -> Worker:
        worker = self.live.get(record.id)
        spec = self.spec(record)
        if worker is not None and (worker.spec.workspace.path, worker.spec.machine.resources) != (
                spec.workspace.path, spec.machine.resources):
            # Its slot or the configuration changed: start a process with the new directory and GPUs.
            self._close_later(self.live.pop(record.id))
            worker = None
        if worker is None:
            worker = self.live[record.id] = self.factory(spec)
        return worker

    async def _run(self, job_id: str) -> None:
        job = self.store.jobs.get(job_id)
        record = self.store.workers.get(job.worker_id)
        state = self.running[job_id]
        retire = record.ephemeral
        try:
            worker = self._worker(record)

            async def on_approval(request: ApprovalRequest) -> str:
                return await self.approvals.request(worker=record, job=job, request=request)

            outcome = await worker.run(frame(job, record), resume=state.resume, on_approval=on_approval)
            artifacts = []
            if outcome.result.artifacts:
                transport = make_transport(worker.spec.machine, excluded_env=worker.spec.excluded_env)
                artifacts = await collect(transport, worker.spec.workspace.path, outcome.result.artifacts)
            status = "interrupted" if state.interrupted or state.stopping else "done"
            retire = retire or state.stopping
            self._finish(job, record, status, result=outcome.result, session=outcome.backend_session_id,
                         artifacts=artifacts, stop=retire, state=state)
        except asyncio.CancelledError:
            if state.stopping:
                self._finish(job, record, "cancelled", error="stopped", stop=True, state=state)
            raise
        except Exception as error:
            status = "interrupted" if state.interrupted or state.stopping else "failed"
            retire = retire or state.stopping
            logger.warning("job %s on %s ended (%s): %s", job_id, record.machine, status, error)
            self._finish(job, record, status, error=str(error)[:2000], stop=retire, state=state)
        finally:
            self.running.pop(job_id, None)
            self.bus.jobs.ring()
        if retire:
            await self._drop(record.id)

    def _finish(self, job: Job, record: WorkerRecord | None, status: str, *, result=None, session: str = "",
                artifacts=(), error: str = "", stop: bool = False, state: Running | None = None) -> None:
        """Record a job's end, its artifacts, and the inbox event for its thread; at most once per job."""
        if state is not None:
            if state.finished:
                return
            state.finished = True
        now = self.clock.now()
        with self.store.transaction():
            self.store.jobs.finish(job.id, status, now, result=result, error=error)
            if record is not None:
                worker_status = "stopped" if stop else "idle"
                self.store.workers.record_result(record.id, result, session, worker_status, now)
            for item in artifacts:
                self.store.artifacts.add(uuid.uuid4().hex, job, record.machine, item.ref, data=item.data,
                                         error=item.error)
            self.store.inbox.add(job.session_id, "worker_result", now, ref=job.id)
        self.bus.thread(job.session_id)

    async def _drop(self, worker_id: str) -> None:
        worker = self.live.pop(worker_id, None)
        if worker is not None:
            await worker.close()

    # ----- control -----

    async def interrupt(self, worker_id: str) -> bool:
        running = next((item for item in self.running.values() if item.worker_id == worker_id), None)
        worker = self.live.get(worker_id)
        if running is None or worker is None:
            return False
        running.interrupted = True
        await worker.interrupt()
        return True

    async def stop(self, worker_id: str) -> None:
        """Cancel queued jobs, interrupt the running one, and retire the worker."""
        now = self.clock.now()
        sessions = set()
        with self.store.transaction():
            for job_id in self.store.jobs.cancel_queued(worker_id, now):
                job = self.store.jobs.get(job_id)
                self.store.inbox.add(job.session_id, "worker_result", now, ref=job_id)
                sessions.add(job.session_id)
            self.store.workers.set_status(worker_id, "stopped", now)
        for session_id in sessions:
            self.bus.thread(session_id)
        running = next((item for item in self.running.values() if item.worker_id == worker_id), None)
        if running is not None:
            running.stopping = True
            worker = self.live.get(worker_id)
            if worker is not None:
                await worker.interrupt()
            done, _ = await asyncio.wait({running.task}, timeout=10)
            if not done:
                running.task.cancel()
                await asyncio.wait({running.task})
        await self._drop(worker_id)

    def busy_by_machine(self) -> dict[str, int]:
        return self.store.workers.busy_by_machine()

    async def close(self) -> None:
        """Shutdown: cancel running jobs (recovery marks them interrupted) and stop every process."""
        tasks = [item.task for item in self.running.values()]
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        workers, self.live = list(self.live.values()), {}
        await asyncio.gather(*(worker.close() for worker in workers), *self._closing, return_exceptions=True)


def frame(job: Job, record: WorkerRecord) -> str:
    """The job prompt: who this worker is, then the parent's brief."""
    role = "" if record.role == "general" else f" Your role: {record.role}."
    return (f"You are worker {record.id} on {record.machine}, workspace {record.workspace}.{role} "
            f"Deliverable: {job.deliverable}.\n\n{job.brief}")
