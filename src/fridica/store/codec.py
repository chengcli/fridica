"""Row ↔ value-type conversion."""

from __future__ import annotations

from dataclasses import asdict
import json
import sqlite3

from ..core.models import (
    Approval, Attachment, FridicaMeta, InboxItem, Job, Message, OutboxItem, StickyContext, ThreadKey, ThreadSession,
    WorkerRecord, WorkerResult,
)


def dumps(value) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def meta_json(meta: FridicaMeta | None) -> str | None:
    return dumps(asdict(meta)) if meta else None


def meta_from(text: str | None) -> FridicaMeta | None:
    return FridicaMeta(**json.loads(text)) if text else None


def message(row: sqlite3.Row) -> Message:
    return Message(
        event_id=row["event_id"], workspace=row["workspace"], channel=row["channel"], ts=row["ts"],
        thread_ts=row["thread_ts"], sender=row["sender"], text=row["text"],
        files=tuple(json.loads(row["files_json"])), source=row["source"], meta=meta_from(row["meta_json"]),
        attachments=tuple(Attachment(**item) for item in json.loads(row["attachments_json"])),
    )


def session(row: sqlite3.Row) -> ThreadSession:
    return ThreadSession(
        id=row["id"], key=ThreadKey(row["workspace"], row["channel"], row["root_ts"]),
        status=row["status"], control=row["control"], pause_reason=row["pause_reason"], turns=row["turns"],
        wait_streak=row["wait_streak"], no_progress=row["no_progress"], last_reply_hash=row["last_reply_hash"],
        reset_at=row["reset_at"], summary=row["summary"], decisions=tuple(json.loads(row["decisions_json"])),
        context=StickyContext(**json.loads(row["context_json"])), debriefed_turn=row["debriefed_turn"],
        last_unsolicited=row["last_unsolicited"], created=row["created"], updated=row["updated"],
        version=row["version"],
    )


def result_json(result: WorkerResult | None) -> str | None:
    return dumps(result.to_dict()) if result else None


def result_from(text: str | None) -> WorkerResult | None:
    return WorkerResult.from_dict(json.loads(text)) if text else None


def worker(row: sqlite3.Row) -> WorkerRecord:
    return WorkerRecord(
        id=row["id"], session_id=row["session_id"], machine=row["machine"], workspace=row["workspace"],
        backend=row["backend"], role=row["role"], ephemeral=bool(row["ephemeral"]),
        backend_session_id=row["backend_session_id"], status=row["status"], summary=row["summary"],
        last_result=result_from(row["last_result_json"]), slot=row["slot"], created=row["created"],
        updated=row["updated"],
    )


def job(row: sqlite3.Row) -> Job:
    return Job(
        id=row["id"], worker_id=row["worker_id"], session_id=row["session_id"], brief=row["brief"],
        join_group=row["join_group"], inbox_id=row["inbox_id"], deliverable=row["deliverable"],
        status=row["status"], attempt=row["attempt"], reported=bool(row["reported"]),
        result=result_from(row["result_json"]), error=row["error"],
        queued_at=row["queued_at"], started_at=row["started_at"], finished_at=row["finished_at"],
    )


def inbox(row: sqlite3.Row) -> InboxItem:
    return InboxItem(
        id=row["id"], session_id=row["session_id"], kind=row["kind"], ref=row["ref"],
        payload=json.loads(row["payload_json"]), state=row["state"], created=row["created"],
    )


def approval(row: sqlite3.Row) -> Approval:
    return Approval(
        id=row["id"], worker_id=row["worker_id"], job_id=row["job_id"], session_id=row["session_id"],
        kind=row["kind"], summary=row["summary"], detail=json.loads(row["detail_json"]), status=row["status"],
        scope=row["scope"], decided_by=row["decided_by"], backend_request_id=row["backend_request_id"],
        created=row["created"], decided_at=row["decided_at"], expires_at=row["expires_at"],
    )


def outbox(row: sqlite3.Row) -> OutboxItem:
    return OutboxItem(
        id=row["id"], idem_key=row["idem_key"], session_id=row["session_id"], kind=row["kind"],
        channel=row["channel"], thread_ts=row["thread_ts"], text=row["text"], meta=meta_from(row["meta_json"]),
        filename=row["filename"], blob=row["blob"], after=row["after"], state=row["state"],
        attempts=row["attempts"], retry_at=row["retry_at"], sent_ts=row["sent_ts"], error=row["error"],
    )
