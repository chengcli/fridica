"""The parent's decision as typed values, and validation against the thread and the machine registry."""

from __future__ import annotations

from dataclasses import dataclass, field

from ..core.errors import MatchError
from ..core.models import ThreadSession, WorkerRecord
from ..machines.match import Placement, Selector, resolve
from ..machines.registry import Registry
from .schemas import BRIEF_CHARS, DELIVERABLES, DETAILS_CHARS, MAX_DECISIONS, NOTE_KINDS, ROLES, SUMMARY_CHARS

LIVE_STATES = ("idle", "queued", "running", "awaiting_approval", "lost", "failed")


@dataclass(frozen=True)
class Reply:
    send: bool = False
    text: str = ""
    details: str = ""
    status: str = "complete"
    discussion: str = "ongoing"

    @property
    def finished(self) -> bool:
        return self.send and self.discussion == "finished" and self.status == "complete"


@dataclass(frozen=True)
class Delegation:
    brief: str
    worker_id: str = ""
    """An existing worker to continue; empty for a new one placed by ``placement``."""
    placement: Placement | None = None
    role: str = "general"
    ephemeral: bool = False
    deliverable: str = "report"


@dataclass(frozen=True)
class WorkerControl:
    worker_id: str
    op: str


@dataclass(frozen=True)
class Action:
    reply: Reply = Reply()
    delegations: tuple[Delegation, ...] = ()
    controls: tuple[WorkerControl, ...] = ()
    context: dict = field(default_factory=dict)
    summary: str = ""
    decisions: tuple[str, ...] = ()
    note: dict = field(default_factory=dict)
    suppressed: bool = False
    """Set when the reply was dropped because it repeated the thread's last reply word for word."""


@dataclass(frozen=True)
class Rules:
    """What validation needs to know about the thread's world."""
    registry: Registry
    session: ThreadSession
    workers: tuple[WorkerRecord, ...]
    may_delegate: bool
    max_delegations: int
    max_workers: int
    busy: dict
    reply_chars: int = 7000


def _text(value, limit: int) -> str:
    return value.strip()[:limit] if isinstance(value, str) else ""


def validate(raw: dict, rules: Rules) -> tuple[Action, list[str]]:
    """Turn the model's JSON into an Action; problems with delegations are returned for one repair round."""
    errors: list[str] = []
    reply_data = raw.get("reply") if isinstance(raw.get("reply"), dict) else {}
    status = reply_data.get("status") if reply_data.get("status") in ("complete", "waiting", "blocked") else "complete"
    discussion = "finished" if reply_data.get("discussion") == "finished" and status == "complete" else "ongoing"
    text = reply_data.get("text") if isinstance(reply_data.get("text"), str) else ""
    details = _text(reply_data.get("details"), DETAILS_CHARS)
    send = reply_data.get("send") is True and bool(text.strip() or details)
    reply = Reply(send=send, text=text.strip() if send else "", details=details if send else "", status=status,
                  discussion=discussion)

    workers = {worker.id: worker for worker in rules.workers}
    delegations: list[Delegation] = []
    new_workers = 0
    sticky = rules.session.context
    items = [item for item in raw.get("delegate", []) if isinstance(item, dict)] if isinstance(raw.get("delegate"), list) else []
    if items and not rules.may_delegate:
        errors.append("delegation is not allowed in this channel; answer directly or say who can run it")
        items = []
    for index, item in enumerate(items[:rules.max_delegations]):
        label = f"delegate[{index}]"
        brief = _text(item.get("brief"), BRIEF_CHARS)
        if not brief:
            errors.append(f"{label}: brief is empty")
            continue
        role = item.get("role") if item.get("role") in ROLES else "general"
        deliverable = item.get("deliverable") if item.get("deliverable") in DELIVERABLES else "report"
        worker_id = _text(item.get("worker_id"), 64)
        if worker_id:
            worker = workers.get(worker_id)
            if worker is None:
                errors.append(f"{label}: worker {worker_id!r} does not belong to this thread; known: {', '.join(workers) or 'none'}")
                continue
            if worker.status == "stopped":
                errors.append(f"{label}: worker {worker_id} was stopped; delegate to a new worker instead")
                continue
            delegations.append(Delegation(brief, worker_id=worker_id, role=worker.role, deliverable=deliverable))
            continue
        tags = tuple(tag for tag in item.get("tags", []) if isinstance(tag, str) and tag) if isinstance(item.get("tags"), list) else ()
        selector = Selector(machine=_text(item.get("machine"), 64), tags=tags, workspace=_text(item.get("workspace"), 64),
                            backend=item.get("backend") if item.get("backend") in ("claude", "codex") else "")
        try:
            placement = resolve(rules.registry, selector, sticky_machine=sticky.machine,
                                sticky_workspace=sticky.workspace, busy=rules.busy)
        except MatchError as error:
            errors.append(f"{label}: {error}" + (f" (candidates: {', '.join(error.candidates)})" if error.candidates else ""))
            continue
        active = sum(1 for worker in rules.workers if worker.status in LIVE_STATES and not worker.ephemeral)
        if not item.get("ephemeral") and active + new_workers >= rules.max_workers:
            errors.append(f"{label}: this thread already has {active} workers (limit {rules.max_workers});"
                          " continue an existing worker by worker_id or stop one first")
            continue
        new_workers += 0 if item.get("ephemeral") else 1
        delegations.append(Delegation(brief, placement=placement, role=role, ephemeral=item.get("ephemeral") is True,
                                      deliverable=deliverable))
    if len(items) > rules.max_delegations:
        errors.append(f"at most {rules.max_delegations} delegations per turn; {len(items) - rules.max_delegations} dropped")

    controls = []
    for item in raw.get("worker_control", []) if isinstance(raw.get("worker_control"), list) else []:
        if isinstance(item, dict) and item.get("op") in ("interrupt", "stop") and item.get("worker_id") in workers:
            controls.append(WorkerControl(item["worker_id"], item["op"]))

    context_data = raw.get("context") if isinstance(raw.get("context"), dict) else {}
    context = {}
    machine = _text(context_data.get("machine"), 64)
    if machine and rules.registry.get(machine) is not None:
        context["machine"] = machine
    workspace = _text(context_data.get("workspace"), 64)
    target = rules.registry.get(context.get("machine") or sticky.machine or rules.registry.default)
    if workspace and target is not None and target.workspace(workspace) is not None:
        context["workspace"] = workspace
    for key in ("repo", "branch"):
        value = _text(context_data.get(key), 200)
        if value:
            context[key] = value

    note_data = raw.get("note") if isinstance(raw.get("note"), dict) else {}
    note = {key: _text(note_data.get(key), 1000) for key in ("repo", "assignee", "next_step", "blocker")
            if _text(note_data.get(key), 1000)}
    note["kind"] = note_data.get("kind") if note_data.get("kind") in NOTE_KINDS else "result"
    decisions = tuple(_text(item, 500) for item in raw.get("decisions", []) if _text(item, 500))[:MAX_DECISIONS] \
        if isinstance(raw.get("decisions"), list) else ()
    action = Action(reply=reply, delegations=tuple(delegations), controls=tuple(controls), context=context,
                    summary=_text(raw.get("summary"), SUMMARY_CHARS), decisions=decisions, note=note)
    return action, errors
