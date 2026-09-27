"""Value types shared by the Slack, thread, parent, and worker layers.

The hierarchy is: one parent agent per owner, one :class:`ThreadSession` per Slack
thread, and any number of workers per thread, each bound to one machine and
workspace. Slack channels are only a namespace; the thread is the unit of state.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field, fields, replace
from typing import Any


@dataclass(frozen=True)
class ThreadKey:
    """Identifies a Slack thread: a top-level message's own ``ts`` becomes the root."""
    workspace: str
    channel: str
    root_ts: str

    @property
    def id(self) -> str:
        return f"{self.workspace}:{self.channel}:{self.root_ts}"

    @classmethod
    def parse(cls, session_id: str) -> ThreadKey:
        workspace, channel, root_ts = session_id.split(":", 2)
        return cls(workspace, channel, root_ts)


@dataclass(frozen=True)
class FridicaMeta:
    """Slack message metadata that marks a post as written by some owner's Fridica.

    Every owner's Fridica reads it for loop protection: the turn counter and status
    are shared across owners, so two agents in one thread cannot ping-pong forever.
    """
    owner: str
    session: str = ""
    turn: int = 0
    status: str = ""
    kind: str = "reply"
    worker: str = ""
    v: int = 2


@dataclass(frozen=True)
class Attachment:
    """A file shared with a message. ``url`` is Slack's private download URL (files.slack.com only)."""
    id: str
    name: str
    mimetype: str = ""
    size: int = 0
    url: str = ""


@dataclass(frozen=True)
class Message:
    event_id: str
    workspace: str
    channel: str
    ts: str
    thread_ts: str | None
    sender: str
    text: str
    files: tuple[str, ...] = ()
    source: str = "socket"
    """socket | catchup | self (our own post, recorded by the outbox)."""
    meta: FridicaMeta | None = None
    attachments: tuple[Attachment, ...] = ()

    @property
    def root_ts(self) -> str:
        return self.thread_ts or self.ts

    @property
    def key(self) -> ThreadKey:
        return ThreadKey(self.workspace, self.channel, self.root_ts)

    @property
    def top_level(self) -> bool:
        return self.thread_ts is None or self.thread_ts == self.ts

    @property
    def generated(self) -> bool:
        return self.meta is not None


@dataclass(frozen=True)
class StickyContext:
    """What a thread is about, so follow-ups need not repeat it; "" means unset."""
    machine: str = ""
    workspace: str = ""
    repo: str = ""
    branch: str = ""
    backend: str = ""

    def merge(self, **changes: str) -> StickyContext:
        return replace(self, **{key: value for key, value in changes.items() if value})


REPLY_STATUSES = ("complete", "waiting", "blocked")
CONTROL_STATES = ("active", "paused", "closed", "archived", "cleaned")


@dataclass(frozen=True)
class ThreadSession:
    id: str
    key: ThreadKey
    status: str = "new"
    """new | complete | waiting | blocked | working: the outcome of the last reply."""
    control: str = "active"
    pause_reason: str = ""
    turns: int = 0
    wait_streak: int = 0
    no_progress: int = 0
    last_reply_hash: str = ""
    reset_at: float = 0.0
    summary: str = ""
    decisions: tuple[str, ...] = ()
    context: StickyContext = StickyContext()
    debriefed_turn: int = 0
    last_unsolicited: float = 0.0
    created: float = 0.0
    updated: float = 0.0
    version: int = 0


@dataclass(frozen=True)
class Change:
    path: str
    change: str = "modified"
    note: str = ""


@dataclass(frozen=True)
class Validation:
    command: str
    outcome: str = "passed"
    detail: str = ""


@dataclass(frozen=True)
class ArtifactRef:
    path: str
    kind: str = "md"
    caption: str = ""


@dataclass(frozen=True)
class MachineState:
    branch: str = ""
    commit: str = ""
    dirty: bool = False
    notes: str = ""


WORKER_STATUSES = ("done", "partial", "failed", "needs_input")


@dataclass(frozen=True)
class WorkerResult:
    """What a worker hands back to its thread: compact, structured, never raw logs."""
    status: str
    summary: str
    changes: tuple[Change, ...] = ()
    validation: tuple[Validation, ...] = ()
    artifacts: tuple[ArtifactRef, ...] = ()
    machine_state: MachineState = MachineState()
    unresolved: tuple[str, ...] = ()
    question: str = ""
    report: str = ""

    def to_dict(self, *, report: bool = True) -> dict[str, Any]:
        data = asdict(self)
        if not report:
            data.pop("report")
        return data

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> WorkerResult:
        return cls(
            status=data["status"], summary=data["summary"],
            changes=tuple(Change(**item) for item in data.get("changes", ())),
            validation=tuple(Validation(**item) for item in data.get("validation", ())),
            artifacts=tuple(ArtifactRef(**item) for item in data.get("artifacts", ())),
            machine_state=MachineState(**data.get("machine_state", {})),
            unresolved=tuple(data.get("unresolved", ())),
            question=data.get("question", ""), report=data.get("report", ""),
        )


WORKER_STATES = ("idle", "queued", "running", "awaiting_approval", "stopped", "failed", "lost")


@dataclass(frozen=True)
class WorkerRecord:
    id: str
    session_id: str
    machine: str
    workspace: str
    backend: str
    role: str = "general"
    ephemeral: bool = False
    backend_session_id: str = ""
    status: str = "idle"
    summary: str = ""
    last_result: WorkerResult | None = None
    slot: int = 0
    """The machine job slot this worker runs in (1-based; 0 until its first job is scheduled)."""
    created: float = 0.0
    updated: float = 0.0


JOB_STATES = ("queued", "running", "done", "failed", "interrupted", "cancelled")


@dataclass(frozen=True)
class Job:
    id: str
    worker_id: str
    session_id: str
    brief: str
    join_group: str = ""
    inbox_id: int | None = None
    deliverable: str = "report"
    status: str = "queued"
    attempt: int = 0
    reported: bool = False
    result: WorkerResult | None = None
    error: str = ""
    queued_at: float = 0.0
    started_at: float = 0.0
    finished_at: float = 0.0


@dataclass(frozen=True)
class InboxItem:
    id: int
    session_id: str
    kind: str
    """message | worker_result | worker_interrupted | owner_instruction | approval | control | timer"""
    ref: str = ""
    payload: dict[str, Any] = field(default_factory=dict)
    state: str = "pending"
    created: float = 0.0


@dataclass(frozen=True)
class Approval:
    id: str
    worker_id: str
    job_id: str
    session_id: str
    kind: str
    """command | file_change | permissions | tool"""
    summary: str
    detail: dict[str, Any] = field(default_factory=dict)
    status: str = "pending"
    scope: str = "once"
    decided_by: str = ""
    backend_request_id: str = ""
    created: float = 0.0
    decided_at: float = 0.0
    expires_at: float = 0.0


OUTBOX_KINDS = ("reply", "notice", "report", "debrief_root", "upload", "approval_notice")


@dataclass(frozen=True)
class OutboxItem:
    idem_key: str
    session_id: str
    kind: str
    channel: str
    thread_ts: str | None
    text: str = ""
    meta: FridicaMeta | None = None
    filename: str = ""
    blob: bytes | None = None
    after: str = ""
    """Idempotency key of an item that must be sent first (for example a reply before its upload)."""
    id: int = 0
    state: str = "pending"
    attempts: int = 0
    retry_at: float = 0.0
    sent_ts: str = ""
    error: str = ""


def field_names(cls: type) -> tuple[str, ...]:
    return tuple(item.name for item in fields(cls))
