"""JSON views of daemon state for the control API."""

from __future__ import annotations

from dataclasses import asdict

from ..core.models import Approval, Job, Message, OutboxItem, ThreadSession, WorkerRecord


def session(item: ThreadSession) -> dict:
    data = asdict(item)
    data["key"] = asdict(item.key)
    data["decisions"] = list(item.decisions)
    return data


def message(item: Message) -> dict:
    return {"event_id": item.event_id, "ts": item.ts, "thread_ts": item.thread_ts, "sender": item.sender,
            "text": item.text, "source": item.source, "generated": item.generated,
            "meta": asdict(item.meta) if item.meta else None}


def worker(item: WorkerRecord, *, live: bool = False, busy: bool = False) -> dict:
    data = asdict(item)
    data["last_result"] = item.last_result.to_dict() if item.last_result else None
    data["process"] = "busy" if busy else "alive" if live else "stopped"
    return data


def job(item: Job) -> dict:
    data = asdict(item)
    data["result"] = item.result.to_dict() if item.result else None
    return data


def approval(item: Approval) -> dict:
    return asdict(item)


def outbox(item: OutboxItem) -> dict:
    data = asdict(item)
    data.pop("blob")
    data["meta"] = asdict(item.meta) if item.meta else None
    data["has_file"] = item.blob is not None
    return data
