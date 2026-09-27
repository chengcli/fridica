"""The WorkerResult contract: its JSON schema, and lenient parsing of what a worker returned.

A worker may read a hundred files and run dozens of commands; what reaches its
thread is this compact structure. Codex receives the schema as the turn's
``outputSchema``; Claude is asked to end with one fenced JSON block.
"""

from __future__ import annotations

import json
import re

from ..core.models import (
    WORKER_STATUSES, ArtifactRef, Change, MachineState, Validation, WorkerResult,
)

SUMMARY_LIMIT = 1500
REPORT_LIMIT = 4000
MAX_ARTIFACTS = 3
MAX_ITEMS = 30
ARTIFACT_KINDS = ("png", "pdf", "md")


def _object(properties: dict) -> dict:
    """A strict object schema (Codex structured output requires every property to be required)."""
    return {"type": "object", "additionalProperties": False, "properties": properties, "required": list(properties)}


STRING = {"type": "string"}
RESULT_SCHEMA = _object({
    "status": {"type": "string", "enum": list(WORKER_STATUSES)},
    "summary": {"type": "string", "maxLength": SUMMARY_LIMIT},
    "changes": {"type": "array", "maxItems": MAX_ITEMS, "items": _object({
        "path": STRING, "change": {"type": "string", "enum": ["added", "modified", "deleted"]}, "note": STRING})},
    "validation": {"type": "array", "maxItems": MAX_ITEMS, "items": _object({
        "command": STRING, "outcome": {"type": "string", "enum": ["passed", "failed", "skipped"]},
        "detail": {"type": "string", "maxLength": 300}})},
    "artifacts": {"type": "array", "maxItems": MAX_ARTIFACTS, "items": _object({
        "path": STRING, "kind": {"type": "string", "enum": list(ARTIFACT_KINDS)}, "caption": STRING})},
    "machine_state": _object({"branch": STRING, "commit": STRING, "dirty": {"type": "boolean"}, "notes": STRING}),
    "unresolved": {"type": "array", "maxItems": MAX_ITEMS, "items": STRING},
    "question": STRING,
    "report": {"type": "string", "maxLength": REPORT_LIMIT},
})

FORMAT_NOTE = f"""When the job is done, end your final message with exactly one fenced ```json block containing a
WorkerResult object and nothing after it. Schema:
{json.dumps(RESULT_SCHEMA, separators=(",", ":"))}
- status: done (finished), partial (some of it), failed (could not), needs_input (you need an answer to continue).
- summary: what you found and did, for the coordinating agent; facts, not narration.
- report: a Slack-ready message in the owner's first-person voice that could be posted as is.
- artifacts: absolute paths of at most {MAX_ARTIFACTS} files you wrote inside the workspace (png, pdf, or md).
- question: set only with needs_input."""

SUMMARIZE_PROMPT = ("Return only the WorkerResult JSON object for the work you just did, in one fenced ```json "
                    "block, with no other text.")

# A fenced block: an opening fence at the start of a line with an optional info string (json, python, …),
# its body, and the next closing fence line. Anchoring to lines keeps consecutive blocks paired correctly.
FENCE = re.compile(r"^```[\w+-]*[ \t]*\n(.*?)\n```[ \t]*$", re.DOTALL | re.MULTILINE)


def parse(text: str) -> WorkerResult | None:
    """The WorkerResult in ``text`` (a bare object or the last fenced block), or None when absent or invalid."""
    candidates = [text.strip()] + [block.strip() for block in reversed(FENCE.findall(text))]
    start = text.rfind("\n{")
    if start >= 0:
        candidates.append(text[start:].strip())
    for candidate in candidates:
        try:
            data = json.loads(candidate)
        except ValueError:
            continue
        result = coerce(data)
        if result is not None:
            return result
    return None


def prose(text: str) -> str:
    """The message text without its WorkerResult block; other fenced blocks (code examples) are kept."""
    for match in reversed(list(FENCE.finditer(text))):
        try:
            data = json.loads(match.group(1))
        except ValueError:
            continue
        if coerce(data) is not None:
            return (text[:match.start()] + text[match.end():]).strip()
    return text.strip()


def coerce(data) -> WorkerResult | None:
    """Validate and bound a decoded object; tolerant of missing optional fields."""
    if not isinstance(data, dict) or data.get("status") not in WORKER_STATUSES:
        return None
    summary = data.get("summary")
    if not isinstance(summary, str) or not summary.strip():
        return None

    def items(key):
        value = data.get(key, [])
        return [item for item in value if isinstance(item, dict)][:MAX_ITEMS] if isinstance(value, list) else []

    def text(value, limit=2000):
        return value[:limit] if isinstance(value, str) else ""

    changes = tuple(Change(text(item.get("path"), 500), item.get("change") if item.get("change") in
                           ("added", "modified", "deleted") else "modified", text(item.get("note"), 500))
                    for item in items("changes") if text(item.get("path")))
    validation = tuple(Validation(text(item.get("command"), 500), item.get("outcome") if item.get("outcome") in
                                  ("passed", "failed", "skipped") else "skipped", text(item.get("detail"), 300))
                       for item in items("validation") if text(item.get("command")))
    artifacts = tuple(ArtifactRef(text(item.get("path"), 1000), item.get("kind"), text(item.get("caption"), 300))
                      for item in items("artifacts")
                      if text(item.get("path")) and item.get("kind") in ARTIFACT_KINDS)[:MAX_ARTIFACTS]
    state = data.get("machine_state") if isinstance(data.get("machine_state"), dict) else {}
    machine_state = MachineState(text(state.get("branch"), 200), text(state.get("commit"), 64),
                                 state.get("dirty") is True, text(state.get("notes"), 1000))
    unresolved = tuple(text(item, 500) for item in data.get("unresolved", []) if isinstance(item, str))[:MAX_ITEMS] \
        if isinstance(data.get("unresolved"), list) else ()
    return WorkerResult(status=data["status"], summary=summary[:SUMMARY_LIMIT], changes=changes,
                        validation=validation, artifacts=artifacts, machine_state=machine_state,
                        unresolved=unresolved, question=text(data.get("question"), 2000),
                        report=text(data.get("report"), REPORT_LIMIT))


def fallback(text: str) -> WorkerResult:
    """A partial result built from free prose when the worker never produced the structure."""
    body = prose(text) or "The worker finished without a report."
    return WorkerResult(status="partial", summary=body[-SUMMARY_LIMIT:], report=body[-REPORT_LIMIT:])
