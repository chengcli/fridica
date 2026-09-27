"""What the rest of Fridica needs from a worker, independent of backend and transport."""

from __future__ import annotations

from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Protocol

from ..core.models import WorkerResult
from ..machines.registry import Machine, Workspace

ALLOW_ONCE = "once"
ALLOW_SESSION = "session"
DENY = "deny"


@dataclass(frozen=True)
class ApprovalRequest:
    """A backend asking permission for something its policy does not already allow."""
    kind: str
    """command | file_change | permissions | tool"""
    summary: str
    detail: dict = field(default_factory=dict)
    backend_request_id: str = ""
    cache_key: str = ""
    """Identifies "the same action" for allow-for-session decisions; "" means never cached."""


ApprovalHandler = Callable[[ApprovalRequest], Awaitable[str]]
"""Returns ALLOW_ONCE, ALLOW_SESSION, or DENY."""


async def deny_all(_request: ApprovalRequest) -> str:
    return DENY


@dataclass(frozen=True)
class WorkerSpec:
    """Everything needed to start one worker process."""
    worker_id: str
    machine: Machine
    workspace: Workspace
    backend: str
    instructions: str
    """Standing instructions for every job (the contract's worker sections and the owner profile)."""
    model: str = ""
    reasoning_effort: str = ""
    job_timeout: float = 4 * 3600.0
    idle_timeout: float = 1800.0
    excluded_env: tuple[str, ...] = ()
    slot: int = 0

    @property
    def create_cwd(self) -> bool:
        """The workspace is a slot subfolder, created on first use."""
        return self.slot > 0 and any(item.subfolders for item in self.machine.workspaces
                                     if item.name == self.workspace.name)

    @property
    def confined(self) -> bool:
        return self.workspace.policy.gpu_confine


@dataclass(frozen=True)
class Outcome:
    result: WorkerResult
    backend_session_id: str


class Worker(Protocol):
    spec: WorkerSpec

    @property
    def alive(self) -> bool: ...

    @property
    def busy(self) -> bool: ...

    async def run(self, brief: str, *, resume: str, on_approval: ApprovalHandler) -> Outcome: ...

    async def interrupt(self) -> None: ...

    async def close(self) -> None: ...


WorkerFactory = Callable[[WorkerSpec], Worker]
