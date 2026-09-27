"""Capture frozen first-pass scheduling, sticky slots and worker prompt/resume data.

This projection intentionally sorts concurrently started worker calls by worker
ID; it compares admission order, not arbitrary task-poll interleavings. Completion,
retry and process-close fault paths have separate Rust regression tests.
"""
import asyncio
from dataclasses import replace
import json
from pathlib import Path
import tempfile
import tomllib

from fridica.config.loader import parse
from fridica.core.bus import Bus
from fridica.core.models import Job, ThreadKey, WorkerRecord
from fridica.store import Store
from fridica.workers.supervisor import Supervisor


class Clock:
    def now(self):
        return 20.0


class Worker:
    def __init__(self, spec, calls):
        self.spec = spec
        self.calls = calls
        self.alive = True
        self.busy = False

    async def run(self, brief, *, resume, on_approval):
        self.busy = True
        self.calls.append({"worker": self.spec.worker_id, "brief": brief, "resume": resume})
        await asyncio.Event().wait()

    async def close(self):
        self.alive = False


def worker(name, machine="gpu", slot=0, jobs=1, **kw):
    return {"id": name, "machine": machine, "workspace": "shared" if machine == "gpu2" else "unique",
            "backend": "codex", "slot": slot, "jobs": jobs, **kw}


async def capture(source, case):
    with tempfile.TemporaryDirectory() as directory:
        store = Store(Path(directory) / "db")
        config = parse(tomllib.loads(source), base=Path(directory))
        config = replace(config, limits=replace(config.limits, max_jobs=case.get("max_jobs", 3)))
        session = store.threads.ensure(ThreadKey("TTEAM", "CROOM", "100.1"), 1.)
        for record in case["workers"]:
            data = dict(record)
            jobs = data.pop("jobs")
            store.workers.add(WorkerRecord(session_id=session.id, **data), 1.)
            for n in range(jobs):
                store.jobs.add(Job(f"{record['id']}-{n}", record['id'], session.id, f"brief {n}"), 1.)
        calls = []
        supervisor = Supervisor(config, store, Bus(), instructions=lambda w: f"rules for {w.role}",
                                factory=lambda spec: Worker(spec, calls), clock=Clock())
        started = supervisor.schedule()
        for _ in range(10):
            await asyncio.sleep(0)
        expected = {
            "started": started,
            "workers": [dict(row) for row in store.db.all("SELECT id,slot,status FROM workers ORDER BY rowid")],
            "jobs": [dict(row) for row in store.db.all("SELECT id,status,attempt FROM jobs ORDER BY rowid")],
            "inbox": [dict(row) for row in store.db.all("SELECT kind,ref FROM thread_inbox ORDER BY id")],
            "calls": sorted(calls, key=lambda row: row["worker"]),
        }
        await supervisor.close()
        store.close()
        return {**case, "expected": expected}


async def main():
    root = Path(__file__).resolve().parents[1]
    source = json.loads((root / "tests/corpus/placement.json").read_text())["source"]
    cases = [
        {"id": "global-machine-worker-limits", "workers": [worker("a", jobs=2), worker("b"), worker("c"), worker("d", "gpu2"), worker("e", "gpu2")]},
        {"id": "sticky-slot-waits", "workers": [worker("a", slot=1), worker("b", slot=1)]},
        {"id": "least-assigned-free-slot", "workers": [worker("idle", slot=1, jobs=0), worker("a")]},
        {"id": "queued-stopped-worker", "workers": [worker("a", status="stopped"), worker("b")]},
        {"id": "removed-machine", "workers": [worker("a", "missing"), worker("b")]},
        {"id": "remembered-session-and-role", "workers": [worker("a", backend_session_id="prior-session", role="reviewer")]},
        {"id": "global-single-job", "max_jobs": 1, "workers": [worker("a"), worker("b", "gpu2")]},
        {"id": "slot-outside-new-range", "workers": [worker("a", slot=9), worker("b", slot=1)]},
    ]
    result = [await capture(source, case) for case in cases]
    (root / "tests/corpus/supervisor.json").write_text(json.dumps(result, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"Captured {len(result)} supervisor scheduling scenarios.")


if __name__ == "__main__":
    asyncio.run(main())
