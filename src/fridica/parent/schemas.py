"""Structured-output schemas for the parent's calls.

Codex's strict structured output needs every property required, no extra
properties, and no ``oneOf``, so optional values are empty strings or lists.
"""

from __future__ import annotations

REPLY_CHARS = 7000
DETAILS_CHARS = 40000
BRIEF_CHARS = 40000
SUMMARY_CHARS = 2000
DIGEST_CHARS = 2500
MAX_DECISIONS = 20
ROLES = ("general", "implementer", "reviewer", "tester")
DELIVERABLES = ("report", "markdown", "figures_pdf")
NOTE_KINDS = ("result", "question", "status", "ack", "correction")


def strict(properties: dict) -> dict:
    return {"type": "object", "additionalProperties": False, "properties": properties, "required": list(properties)}


def string(description: str = "", **extra) -> dict:
    return {"type": "string", **({"description": description} if description else {}), **extra}


def enum(values, description: str = "") -> dict:
    return string(description, enum=list(values))


TRIAGE_SCHEMA = strict({"decision": enum(("ignore", "observe", "respond"))})

REPLY = strict({
    "send": {"type": "boolean"},
    "text": string(f"The Slack reply in the owner's voice, at most {REPLY_CHARS} characters; empty when send is false."),
    "details": string(f"Empty, or a Markdown document of at most {DETAILS_CHARS} characters uploaded as a file."),
    "status": enum(("complete", "waiting", "blocked")),
    "discussion": enum(("ongoing", "finished")),
})

DELEGATION = strict({
    "worker_id": string("An existing worker of this thread to continue, or empty for a new worker."),
    "machine": string("For a new worker: a machine name from the machines field, or empty to choose by tags."),
    "tags": {"type": "array", "items": {"type": "string"}, "description": "For a new worker: required capability tags."},
    "workspace": string("For a new worker: a workspace name on the chosen machine, or empty for the thread's."),
    "backend": enum(("claude", "codex", ""), "For a new worker: empty for the machine's default."),
    "role": enum(ROLES),
    "ephemeral": {"type": "boolean", "description": "Retire the worker after this one job."},
    "brief": string(f"The self-contained job description, at most {BRIEF_CHARS} characters."),
    "deliverable": enum(DELIVERABLES),
})

WORKER_CONTROL = strict({"worker_id": string(), "op": enum(("interrupt", "stop"))})

CONTEXT = strict({name: string("Empty leaves it unchanged.") for name in ("machine", "workspace", "repo", "branch")})

NOTE = strict({
    "kind": enum(NOTE_KINDS, "What this reply is: a result, a question, a status update, an acknowledgment, or a correction."),
    "repo": string("A repository name from the repositories field, or empty."),
    "assignee": string("A Slack member ID (U…) of who acts next, or empty."),
    "next_step": string("Empty leaves it unchanged."),
    "blocker": string("Empty leaves it unchanged."),
})

ACTION_SCHEMA = strict({
    "reply": REPLY,
    "delegate": {"type": "array", "items": DELEGATION},
    "worker_control": {"type": "array", "items": WORKER_CONTROL},
    "context": CONTEXT,
    "summary": string(f"The thread's rolling summary (goal, decisions, open items), at most {SUMMARY_CHARS} characters; empty keeps it."),
    "decisions": {"type": "array", "items": {"type": "string"}, "description": "New decisions made in this turn only."},
    "note": NOTE,
})

DEBRIEF_SCHEMA = strict({"debrief": string("A plain-text debrief of the finished discussion for the channel.")})
