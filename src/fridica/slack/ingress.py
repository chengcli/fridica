"""Slack Events API payloads → :class:`Message`."""

from __future__ import annotations

import math
import re

from ..core.models import Message
from .render import parse_metadata

TEXT_LIMIT = 40000
TS = re.compile(r"\d+\.\d+")


def normalize(payload: dict, *, source: str = "socket") -> Message | None:
    """The Message in an ``event_callback`` payload, or None for anything Fridica does not handle.

    A message posted through a user token by an app that also has a bot user carries
    both ``user`` and ``bot_id`` (this is how other owners' Fridica replies arrive);
    it belongs to that user. Only messages without ``user`` are dropped as bot posts.
    """
    if not isinstance(payload, dict) or payload.get("type") != "event_callback":
        return None
    event = payload.get("event")
    if not isinstance(event, dict) or event.get("type") != "message":
        return None
    if event.get("subtype") not in (None, "file_share", "thread_broadcast"):
        return None
    fields = [payload.get("event_id"), payload.get("team_id"), event.get("channel"), event.get("user"), event.get("ts")]
    if any(not isinstance(value, str) or not value for value in fields):
        return None
    text = event.get("text") if isinstance(event.get("text"), str) else ""
    files = tuple(item.get("name", "") for item in event.get("files", []) if isinstance(item, dict)) \
        if isinstance(event.get("files"), list) else ()
    if not text and not files:
        return None
    ts = event["ts"]
    thread_ts = event.get("thread_ts")
    if not TS.fullmatch(ts) or not math.isfinite(float(ts)):
        return None
    if thread_ts is not None and (not isinstance(thread_ts, str) or not TS.fullmatch(thread_ts)):
        return None
    return Message(event_id=payload["event_id"], workspace=payload["team_id"], channel=event["channel"], ts=ts,
                   thread_ts=thread_ts if thread_ts != ts else None, sender=event["user"], text=text[:TEXT_LIMIT],
                   files=files, source=source, meta=parse_metadata(event.get("metadata")))


def dropped_mention(payload: object, owner: str) -> str | None:
    """Describe a rejected event that @mentions the owner (such drops are otherwise invisible)."""
    if not isinstance(payload, dict):
        return None
    event = payload.get("event")
    if not isinstance(event, dict) or event.get("type") != "message":
        return None
    text = event.get("text")
    if not isinstance(text, str) or f"<@{owner}>" not in text:
        return None
    return (f"event {payload.get('event_id')} subtype={event.get('subtype')} user={'set' if event.get('user') else 'missing'} "
            f"bot_id={'set' if event.get('bot_id') else 'none'} ts={event.get('ts')}")
