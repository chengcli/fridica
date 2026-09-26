"""What the parent sees for one call: compact, bounded, and never raw worker output."""

from __future__ import annotations

from ..config.schema import Config
from ..core.models import Message, ThreadSession
from ..parent.prompts import ParentContext
from ..store import Store

HISTORY_LIMIT = 60
CHANNEL_LIMIT = 10
CHANNEL_CHARS = 4000


def message_view(message: Message, attachments: list[dict] | None = None) -> dict:
    data = {"event_id": message.event_id, "ts": message.ts, "sender": message.sender, "text": message.text}
    if message.files:
        data["files"] = list(message.files)
    if attachments:
        data["attachments"] = attachments
    if message.meta:
        data["from_agent"] = {"owner": message.meta.owner, "status": message.meta.status, "kind": message.meta.kind}
    return data


def _marker(kept: int, total: int) -> str:
    return f"\n[… truncated to fit the context budget: first {kept:,} of {total:,} characters]"


def fit_attachments(views: list[dict], remaining: int) -> int:
    """Cut attached text to what is left of the context budget, in order; returns what is left afterwards.

    A cut text keeps its marker inside the budget, so the rendered text never exceeds ``remaining``.
    """
    for view in views:
        text = view.get("text")
        if not isinstance(text, str):
            continue
        if len(text) <= remaining:
            remaining -= len(text)
            continue
        kept = remaining - len(_marker(remaining, len(text)))
        kept = max(kept - (len(_marker(max(kept, 0), len(text))) - len(_marker(remaining, len(text)))), 0)
        if kept <= 0 or remaining <= 0:
            view.pop("text")
            view["note"] = "not included: over the context budget (context_chars)"
            remaining = 0
            continue
        view["text"] = text[:kept] + _marker(kept, len(text))
        view["truncated"] = True
        remaining = 0
    return remaining


def bounded(items: list[dict], budget: int) -> tuple[dict, ...]:
    """The newest items that fit in ``budget`` characters of text, oldest first."""
    kept, used = [], 0
    for item in reversed(items):
        size = len(item.get("text", "")) + 80
        if used + size > budget and kept:
            break
        kept.append(item)
        used += size
    return tuple(reversed(kept))


def worker_view(record) -> dict:
    return {"worker_id": record.id, "machine": record.machine, "workspace": record.workspace, "backend": record.backend,
            "role": record.role, "ephemeral": record.ephemeral, "status": record.status, "summary": record.summary,
            "last_result": record.last_result.to_dict(report=False) if record.last_result else None}


def build(store: Store, config: Config, session: ThreadSession, trigger: dict, *, repositories: tuple[dict, ...],
          busy: dict[str, int], linked: tuple[dict, ...] = (), github: tuple[dict, ...] = (),
          attachments: dict[str, list[dict]] | None = None) -> ParentContext:
    budget = config.parent.context_chars
    attachments = attachments or {}
    history = [message_view(item, attachments.get(item.event_id))
               for item in store.messages.thread(session.key, limit=HISTORY_LIMIT)]
    channel = ()
    if session.turns == 0:
        recent = store.messages.channel_recent(session.key.workspace, session.key.channel, session.key.root_ts,
                                               limit=CHANNEL_LIMIT)
        channel = bounded([message_view(item) for item in recent], CHANNEL_CHARS)
    history = bounded(history, budget // 2)
    # Attached text shares context_chars with the thread: what history leaves, the trigger's files first.
    remaining = budget - sum(len(item.get("text", "")) + 80 for item in history)
    message = trigger.get("message")
    if isinstance(message, dict) and message.get("attachments"):
        trigger = {**trigger, "message": {**message, "attachments": [dict(view) for view in message["attachments"]]}}
        remaining = fit_attachments(trigger["message"]["attachments"], remaining)
    history = tuple({**item, "attachments": [dict(view) for view in item["attachments"]]} if item.get("attachments")
                    else item for item in history)
    for item in reversed(history):  # newest first
        if item.get("attachments"):
            remaining = fit_attachments(item["attachments"], remaining)
    return ParentContext(
        owner=config.owner.slack_user, profile=config.owner.profile, session=session, trigger=trigger,
        history=history, channel=channel, linked=linked, github=github,
        workers=tuple(worker_view(record) for record in store.workers.for_session(session.id)),
        machines=tuple(config.machines.payload(busy)), repositories=repositories,
        notes=store.notes.current(session.id)[1], delegation_allowed=config.slack.may_delegate(session.key.channel),
        limits={"max_delegations_per_turn": config.limits.max_delegations_per_turn,
                "max_workers_per_thread": config.limits.max_workers_per_thread})
