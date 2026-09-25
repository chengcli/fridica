"""Pending and decided approval requests raised by workers."""

from __future__ import annotations

from ..core.models import Approval
from . import codec
from .db import Database


class Approvals:
    def __init__(self, db: Database):
        self.db = db

    def add(self, approval: Approval) -> None:
        self.db.execute(
            "INSERT INTO approvals (id, worker_id, job_id, session_id, backend_request_id, kind, summary, detail_json,"
            " status, scope, decided_by, created, decided_at, expires_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            (approval.id, approval.worker_id, approval.job_id, approval.session_id, approval.backend_request_id,
             approval.kind, approval.summary, codec.dumps(approval.detail), approval.status, approval.scope,
             approval.decided_by, approval.created, approval.decided_at, approval.expires_at),
        )

    def get(self, approval_id: str) -> Approval | None:
        row = self.db.one("SELECT * FROM approvals WHERE id=?", (approval_id,))
        return codec.approval(row) if row else None

    def decide(self, approval_id: str, status: str, scope: str, decided_by: str, now: float) -> bool:
        cursor = self.db.execute(
            "UPDATE approvals SET status=?, scope=?, decided_by=?, decided_at=? WHERE id=? AND status='pending'",
            (status, scope, decided_by, now, approval_id))
        return cursor.rowcount == 1

    def list(self, *, statuses: tuple[str, ...] = ("pending",), limit: int = 100) -> list[Approval]:
        rows = self.db.all(
            f"SELECT * FROM approvals WHERE status IN ({','.join('?' * len(statuses))}) ORDER BY created DESC LIMIT ?",
            (*statuses, limit))
        return [codec.approval(row) for row in rows]
