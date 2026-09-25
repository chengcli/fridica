"""Turning parent output into Slack messages, and Fridica's message metadata."""

from __future__ import annotations

from collections.abc import Iterable
import re

from ..core.models import FridicaMeta, Message

EVENT_TYPE = "fridica_message"
PERMALINK = re.compile(r"https://[A-Za-z0-9.-]+\.slack\.com/archives/([CG][A-Z0-9]{2,})/p(\d+)(\d{6})(?:\?([^\s|>]*))?")
LINK_LIMIT = 3
CONTINUED = "\n\n_The full reply is in the attached details file._"
ATTACHED = "The reply is in the attached details file."
STATUSES = ("complete", "waiting", "blocked")


def metadata(meta: FridicaMeta) -> dict:
    """Slack message metadata; ``task_id`` keeps older Fridica versions' loop protection working."""
    return {"event_type": EVENT_TYPE, "event_payload": {
        "v": meta.v, "owner": meta.owner, "session": meta.session, "task_id": meta.session, "turn": meta.turn,
        "status": meta.status, "kind": meta.kind, "worker": meta.worker}}


def parse_metadata(value) -> FridicaMeta | None:
    """The FridicaMeta in an event's metadata (v2, or v1 with task_id), or None when it is not ours."""
    if not isinstance(value, dict) or value.get("event_type") != EVENT_TYPE:
        return None
    data = value.get("event_payload") if isinstance(value.get("event_payload"), dict) else {}
    turn = data.get("turn", 0)
    if isinstance(turn, bool) or not isinstance(turn, int) or turn < 0:
        turn = 0

    def text(key, limit=128):
        item = data.get(key)
        return item[:limit] if isinstance(item, str) else ""

    return FridicaMeta(owner=text("owner", 32), session=text("session") or text("task_id"), turn=min(turn, 10000),
                       status=data.get("status") if data.get("status") in STATUSES else "",
                       kind=text("kind", 32) or "reply", worker=text("worker", 64),
                       v=data.get("v") if data.get("v") in (1, 2) else 1)


def permalinks(text: str) -> list[tuple[str, str, str, str | None]]:
    """Distinct Slack message links in ``text`` as (link, channel, ts, thread root or None), at most LINK_LIMIT."""
    found = []
    for match in PERMALINK.finditer(text):
        channel, timestamp = match.group(1), f"{match.group(2)}.{match.group(3)}"
        root = re.search(r"(?:^|&)thread_ts=(\d+\.\d+)", match.group(4) or "")
        entry = (match.group(0).split("?")[0], channel, timestamp, root.group(1) if root else None)
        if all(item[1:3] != entry[1:3] for item in found):
            found.append(entry)
    return found[:LINK_LIMIT]


def split_message(text: str, limit: int) -> list[str]:
    """``text`` in consecutive parts of at most ``limit`` characters, split at paragraphs, then lines, then words."""
    if limit < 1:
        raise ValueError("split_message needs a positive limit")
    parts, rest = [], text.strip()
    while len(rest) > limit:
        window = rest[:limit + 1]
        cut = next((index for index in (window.rfind(mark) for mark in ("\n\n", "\n", " ")) if index > limit // 3), limit)
        parts.append(rest[:cut].rstrip())
        rest = rest[cut:].lstrip()
    return [*parts, rest] if rest else parts


def fit_reply(text: str, details: str, limit: int) -> tuple[str, str]:
    """A reply too long for one message keeps its opening in the thread; the whole text heads the details file."""
    text = text.strip()
    if not text and details:
        return ATTACHED, details
    if len(text) <= limit:
        return text, details
    full = text + (f"\n\n---\n\n{details}" if details else "")
    if limit <= len(CONTINUED) + 20:
        return text[:max(limit, 1)], full  # too little room for a pointer; keep the configured maximum
    return split_message(text, limit - len(CONTINUED))[0] + CONTINUED, full


def mentions(text: str, participants: Iterable[str]) -> str:
    """Render bare member IDs of known participants as <@ID>, leaving code, links, and existing mentions alone."""
    participants = set(participants)
    protected = re.compile(r"(```[\s\S]*?```|`[^`]*`|<[^>]*>|https?://[^\s<>]+)")
    identifier = re.compile(r"(?<![\w@])([UW][A-Z0-9]+)(?!\w)")
    pieces = protected.split(text)
    for index in range(0, len(pieces), 2):
        pieces[index] = identifier.sub(lambda match: f"<@{match[0]}>" if match[0] in participants else match[0],
                                       pieces[index])
    return "".join(pieces)


def participants(owner: str, messages: Iterable[Message]) -> set[str]:
    people = {owner}
    for message in messages:
        people.add(message.sender)
        people.update(re.findall(r"<@([UW][A-Z0-9]+)>", message.text))
    return people


def reply_text(text: str, details: str, *, status: str, requester: str, people: set[str],
               limit: int) -> tuple[str, str]:
    """The final thread text and details: mentions rendered, the requester addressed when waiting, overflow moved."""
    mention = f"<@{requester}>"
    waiting = status == "waiting" and bool(requester)
    text, details = fit_reply(mentions(text, people), details, limit - (len(mention) + 1 if waiting else 0))
    if waiting and mention not in text:
        text = f"{mention} {text}"
    return text, details
