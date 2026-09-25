"""Participation and loop-protection rules, as pure functions of a message and its thread session.

Several owners' Fridicas may share a thread, so every rule here also protects
against two agents talking to each other forever: the turn counter travels in
Slack metadata, finished peer replies are ignored unless they address us, and a
thread pauses after too many clarifying questions or turns without progress.
"""

from __future__ import annotations

from dataclasses import dataclass, replace
import hashlib

from ..config.schema import Limits
from ..core.models import Message, ThreadSession

MAX_DECISIONS = 20


@dataclass(frozen=True)
class Verdict:
    kind: str
    """ignore | observe | notice | triage | respond"""
    reason: str
    turn: int = 0


def mentions(message: Message, owner: str) -> bool:
    return f"<@{owner}>" in message.text


def gate(message: Message, session: ThreadSession, *, owner: str, limits: Limits, general_messages: bool,
         cooling: bool, observe_only: bool = False, resumed: bool = False) -> Verdict:
    """What to do with one message, before any model is called."""
    if observe_only:
        return Verdict("observe", "observe-only mode")
    if session.control != "active":
        return Verdict("observe", f"thread is {session.control}")
    if not resumed and session.reset_at and float(message.ts) <= session.reset_at:
        return Verdict("observe", "sent before the thread was resumed")
    if message.sender == owner:
        return Verdict("ignore", "our own post") if message.generated else Verdict("observe", "the owner wrote")
    mentioned = mentions(message, owner) or resumed
    waiting = session.status == "waiting"
    # Peers' turn counters always propagate; nothing resets them now that threads have no turn limit.
    peer_turn = message.meta.turn if message.meta else 0
    turn = max(session.turns, peer_turn) + 1
    if message.generated:
        if message.meta.kind == "debrief_root":
            # Another owner's channel debrief is closing text; answering it would start a new exchange.
            return Verdict("ignore", "another agent's debrief")
        if message.meta.status in ("complete", "blocked") and not (mentioned or waiting):
            return Verdict("ignore", "another agent finished its reply")
        if not (mentioned or waiting):
            return Verdict("ignore", "another agent's message not addressed to us")
    if session.status == "blocked":
        return Verdict("notice", "blocked thread", turn) if mentioned else Verdict("observe", "blocked thread")
    if mentioned or waiting:
        return Verdict("respond", "addressed" if mentioned else "answering our question", turn)
    if message.generated:
        return Verdict("ignore", "another agent's message")
    if session.turns > 0:
        return Verdict("triage", "follow-up in a thread we are part of", turn)
    if general_messages and not cooling:
        return Verdict("triage", "unaddressed message", turn)
    return Verdict("observe", "not addressed")


def reply_hash(text: str) -> str:
    normalized = " ".join(text.casefold().split())
    return hashlib.sha256(normalized.encode()).hexdigest() if normalized else ""


def advance(session: ThreadSession, *, send: bool, status: str, text: str, turn: int, delegated: bool,
            working: bool, note_kind: str, summary: str, decisions: tuple[str, ...], context: dict,
            limits: Limits) -> ThreadSession:
    """The session after a reply (or a decision not to reply), including loop and stall pauses."""
    digest = reply_hash(text) if send else ""
    quiet = (not send and not delegated) or (send and digest == session.last_reply_hash) or note_kind == "ack"
    updated = replace(
        session,
        turns=max(session.turns, turn) if send else session.turns,
        wait_streak=(session.wait_streak + 1 if status == "waiting" else 0) if send else session.wait_streak,
        no_progress=session.no_progress + 1 if quiet else 0,
        last_reply_hash=digest if send else session.last_reply_hash,
        status="working" if delegated or working else (status if send else session.status),
        summary=summary or session.summary,
        decisions=(session.decisions + decisions)[-MAX_DECISIONS:],
        context=session.context.merge(**context),
    )
    if updated.wait_streak >= limits.max_wait_replies:
        updated = replace(updated, control="paused", pause_reason=(
            f"{limits.max_wait_replies} consecutive replies needed more information; possible conversation loop."))
    elif updated.no_progress >= limits.max_no_progress:
        updated = replace(updated, control="paused",
                          pause_reason=f"{limits.max_no_progress} turns without progress; review before continuing.")
    return updated
