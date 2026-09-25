"""Workers, their jobs, and the artifacts jobs produce."""

from __future__ import annotations

from ..core.models import ArtifactRef, Job, WorkerRecord, WorkerResult
from . import codec
from .db import Database


class Workers:
    def __init__(self, db: Database):
        self.db = db

    def add(self, worker: WorkerRecord, now: float) -> WorkerRecord:
        self.db.execute(
            "INSERT INTO workers (id, session_id, machine, workspace, backend, role, ephemeral, backend_session_id,"
            " status, summary, slot, created, updated) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (worker.id, worker.session_id, worker.machine, worker.workspace, worker.backend, worker.role,
             int(worker.ephemeral), worker.backend_session_id, worker.status, worker.summary, worker.slot, now, now),
        )
        return self.get(worker.id)

    def get(self, worker_id: str) -> WorkerRecord | None:
        row = self.db.one("SELECT * FROM workers WHERE id=?", (worker_id,))
        return codec.worker(row) if row else None

    def for_session(self, session_id: str) -> list[WorkerRecord]:
        return [codec.worker(row) for row in self.db.all(
            "SELECT * FROM workers WHERE session_id=? ORDER BY created, rowid", (session_id,))]

    def all(self, *, statuses: tuple[str, ...] | None = None) -> list[WorkerRecord]:
        sql, parameters = "SELECT * FROM workers", ()
        if statuses:
            sql += f" WHERE status IN ({','.join('?' * len(statuses))})"
            parameters = statuses
        return [codec.worker(row) for row in self.db.all(sql + " ORDER BY updated DESC", parameters)]

    def set_status(self, worker_id: str, status: str, now: float) -> None:
        self.db.execute("UPDATE workers SET status=?, updated=? WHERE id=?", (status, now, worker_id))

    def set_slot(self, worker_id: str, slot: int) -> None:
        self.db.execute("UPDATE workers SET slot=? WHERE id=?", (slot, worker_id))

    def record_result(self, worker_id: str, result: WorkerResult | None, backend_session_id: str, status: str,
                      now: float) -> None:
        """Keep the worker's latest compact result and backend session for the next job."""
        summary = result.summary if result else ""
        self.db.execute(
            "UPDATE workers SET last_result_json=COALESCE(?, last_result_json), summary=CASE WHEN ?='' THEN summary"
            " ELSE ? END, backend_session_id=CASE WHEN ?='' THEN backend_session_id ELSE ? END, status=?, updated=?"
            " WHERE id=?",
            (codec.result_json(result), summary, summary, backend_session_id, backend_session_id, status, now,
             worker_id),
        )

    def busy_by_machine(self) -> dict[str, int]:
        rows = self.db.all("SELECT machine, COUNT(*) FROM jobs JOIN workers ON workers.id=jobs.worker_id"
                           " WHERE jobs.status IN ('queued','running') GROUP BY machine")
        return {row[0]: row[1] for row in rows}


class Jobs:
    def __init__(self, db: Database):
        self.db = db

    def add(self, job: Job, now: float) -> Job:
        self.db.execute(
            "INSERT INTO jobs (id, worker_id, session_id, inbox_id, join_group, brief, deliverable, status, attempt,"
            " queued_at) VALUES (?,?,?,?,?,?,?,?,?,?)",
            (job.id, job.worker_id, job.session_id, job.inbox_id, job.join_group, job.brief, job.deliverable,
             job.status, job.attempt, now),
        )
        return self.get(job.id)

    def get(self, job_id: str) -> Job | None:
        row = self.db.one("SELECT * FROM jobs WHERE id=?", (job_id,))
        return codec.job(row) if row else None

    def queued(self) -> list[Job]:
        return [codec.job(row) for row in self.db.all(
            "SELECT * FROM jobs WHERE status='queued' ORDER BY queued_at, rowid")]

    def running(self) -> list[Job]:
        return [codec.job(row) for row in self.db.all("SELECT * FROM jobs WHERE status='running' ORDER BY started_at")]

    def for_worker(self, worker_id: str) -> list[Job]:
        return [codec.job(row) for row in self.db.all(
            "SELECT * FROM jobs WHERE worker_id=? ORDER BY queued_at, rowid", (worker_id,))]

    def group(self, join_group: str) -> list[Job]:
        return [codec.job(row) for row in self.db.all(
            "SELECT * FROM jobs WHERE join_group=? ORDER BY queued_at, rowid", (join_group,))]

    def active_for_worker(self, worker_id: str) -> Job | None:
        row = self.db.one("SELECT * FROM jobs WHERE worker_id=? AND status IN ('queued','running')"
                          " ORDER BY queued_at LIMIT 1", (worker_id,))
        return codec.job(row) if row else None

    def start(self, job_id: str, now: float) -> bool:
        cursor = self.db.execute(
            "UPDATE jobs SET status='running', started_at=?, attempt=attempt+1 WHERE id=? AND status='queued'",
            (now, job_id))
        return cursor.rowcount == 1

    def finish(self, job_id: str, status: str, now: float, *, result: WorkerResult | None = None,
               error: str = "") -> None:
        self.db.execute("UPDATE jobs SET status=?, result_json=?, error=?, finished_at=? WHERE id=?",
                        (status, codec.result_json(result), error, now, job_id))

    def mark_reported(self, job_ids: list[str]) -> None:
        self.db.execute(f"UPDATE jobs SET reported=1 WHERE id IN ({','.join('?' * len(job_ids))})", tuple(job_ids))

    def active_in_session(self, session_id: str) -> list[Job]:
        return [codec.job(row) for row in self.db.all(
            "SELECT * FROM jobs WHERE session_id=? AND status IN ('queued','running') ORDER BY queued_at", (session_id,))]

    def cancel_queued(self, worker_id: str, now: float) -> list[str]:
        rows = self.db.all("SELECT id FROM jobs WHERE worker_id=? AND status='queued'", (worker_id,))
        self.db.execute("UPDATE jobs SET status='cancelled', finished_at=? WHERE worker_id=? AND status='queued'",
                        (now, worker_id))
        return [row[0] for row in rows]


class Artifacts:
    def __init__(self, db: Database):
        self.db = db

    def add(self, artifact_id: str, job: Job, machine: str, ref: ArtifactRef, *, data: bytes | None,
            error: str = "") -> None:
        """Record a file a job produced, with its validated bytes, or the reason it was rejected."""
        self.db.execute(
            "INSERT INTO artifacts (id, job_id, session_id, machine, path, kind, caption, size, blob, status, error)"
            " VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            (artifact_id, job.id, job.session_id, machine, ref.path, ref.kind, ref.caption, len(data or b""), data,
             "ready" if data is not None else "rejected", error))

    def for_job(self, job_id: str) -> list[dict]:
        return [dict(row) for row in self.db.all("SELECT * FROM artifacts WHERE job_id=? ORDER BY rowid", (job_id,))]
