"""The approval broker: persist a worker's request, wait for the owner's decision, time out to deny.

A job waiting for approval holds its worker (and a machine slot) until someone
decides in the dashboard or with ``fridica approvals``; after
``policy.approval_timeout`` the request is denied and the worker continues without it.
"""

from __future__ import annotations

import asyncio
import logging
import uuid

from ..config.schema import Config
from ..core.clock import Clock
from ..core.models import Approval, Job, WorkerRecord
from ..store import Store
from ..workers.protocol import ALLOW_ONCE, ALLOW_SESSION, DENY, ApprovalRequest
from . import rules

logger = logging.getLogger(__name__)
DECISIONS = {ALLOW_ONCE: ("approved", "once"), ALLOW_SESSION: ("approved", "session"), DENY: ("denied", "once")}


class ApprovalBroker:
    def __init__(self, config: Config, store: Store, *, clock: Clock | None = None, notify=None):
        self.config = config
        self.store = store
        self.clock = clock or Clock()
        self.notify = notify
        """Optional callable(Approval) for desktop notifications or a Slack notice."""
        self.waiters: dict[str, asyncio.Future] = {}

    def policy_for(self, worker: WorkerRecord):
        machine = self.config.machines.get(worker.machine)
        workspace = machine.workspace(worker.workspace) if machine else None
        return workspace.policy if workspace else self.config.policy

    async def request(self, *, worker: WorkerRecord, job: Job, request: ApprovalRequest) -> str:
        policy = self.policy_for(worker)
        now = self.clock.now()
        automatic = rules.decide(policy, request)
        if automatic is not None:
            self.store.audit.record("policy", "approval." + ("allow" if automatic != DENY else "deny"), now,
                                    target=worker.id, details={"summary": request.summary})
            return automatic
        approval = Approval(id=uuid.uuid4().hex[:12], worker_id=worker.id, job_id=job.id, session_id=job.session_id,
                            kind=request.kind, summary=request.summary, detail=request.detail,
                            backend_request_id=request.backend_request_id, created=now,
                            expires_at=now + policy.approval_timeout)
        future = asyncio.get_running_loop().create_future()
        self.waiters[approval.id] = future
        with self.store.transaction():
            self.store.approvals.add(approval)
            self.store.workers.set_status(worker.id, "awaiting_approval", now)
        logger.info("worker %s on %s asks: %s (approval %s)", worker.id, worker.machine, request.summary, approval.id)
        if self.notify is not None:
            try:
                self.notify(approval)
            except Exception:
                logger.exception("approval notification failed")
        try:
            return await asyncio.wait_for(asyncio.shield(future), policy.approval_timeout)
        except TimeoutError:
            self._settle(approval.id, DENY, "timeout", status="expired")
            return DENY
        except asyncio.CancelledError:
            # The job was interrupted or stopped while waiting; nobody can act on this request any more.
            self._settle(approval.id, DENY, "interrupted", status="expired")
            raise
        finally:
            self.waiters.pop(approval.id, None)
            self.store.workers.set_status(worker.id, "running", self.clock.now())

    def decide(self, approval_id: str, decision: str, actor: str) -> bool:
        """Record the owner's decision and wake the waiting worker; False if already decided or unknown."""
        if decision not in DECISIONS:
            raise ValueError("decision must be once, session, or deny")
        return self._settle(approval_id, decision, actor)

    def _settle(self, approval_id: str, decision: str, actor: str, *, status: str | None = None) -> bool:
        stored, scope = DECISIONS[decision]
        now = self.clock.now()
        with self.store.transaction():
            changed = self.store.approvals.decide(approval_id, status or stored, scope, actor, now)
            if changed:
                self.store.audit.record(actor, f"approval.{status or stored}", now, target=approval_id,
                                        details={"scope": scope})
        waiter = self.waiters.get(approval_id)
        if changed and waiter is not None and not waiter.done():
            waiter.set_result(decision)
        return changed
