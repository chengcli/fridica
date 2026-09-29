"""Capture clean/restore state projections from frozen Python, without adapters.

The actor runs directly with an empty worker registry and no model/Slack calls.
Rust compares the declared legacy state, not the newer stop-intent or attention
bookkeeping. The fixtures contain synthetic text and attachment addresses only.
"""
import asyncio
import json
from pathlib import Path
import tempfile
from types import SimpleNamespace

from fridica.store import Store
from fridica.threads.actor import ThreadActor

ROOT = Path(__file__).resolve().parents[1]
SESSION = "TTEAM:CROOM:100.1"
SEED = [
    ["INSERT INTO threads(id,workspace,channel,root_ts,status,control,pause_reason,turns,wait_streak,no_progress,summary,decisions_json,context_json,reset_at,created,updated) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
     [SESSION, "TTEAM", "CROOM", "100.1", "blocked", "paused", "Owner pause", 4, 2, 1, "Local summary", '["Use feature branch"]', '{"repo":"owner/repo"}', 50., 10., 20.]],
    ["INSERT INTO messages(event_id,workspace,channel,root_ts,ts,sender,text,files_json,attachments_json,source,received_at) VALUES(?,?,?,?,?,?,?,?,?,?,?)",
     ["old-event", "TTEAM", "CROOM", "100.1", "100.1", "UALICE", "Old message", '["old.diff"]', '[{"id":"FOLD","name":"old.diff","url":"https://files.slack.com/synthetic"}]', "socket", 20.]],
    ["INSERT INTO thread_inbox(session_id,kind,ref,payload_json,state,created) VALUES(?,?,?,?,?,?)",
     [SESSION, "owner_instruction", "old-instruction", '{"text":"Old instruction"}', "done", 20.]],
]


def projection(store):
    session = store.threads.get(SESSION)
    return {
        "thread": {key: getattr(session, key) for key in (
            "control", "status", "summary", "turns", "wait_streak", "no_progress", "reset_at")},
        "decisions": list(session.decisions),
        "context_repo": session.context.repo,
        "messages": [dict(row) for row in store.db.all(
            "SELECT event_id,text,files_json,attachments_json FROM messages ORDER BY id")],
        "instruction_text": store.inbox.instructions(SESSION)[0]["text"],
        "message_inbox_count": store.db.one("SELECT count(*) FROM thread_inbox WHERE kind='message'")[0],
    }


async def capture(actions):
    with tempfile.TemporaryDirectory() as tmp:
        store = Store(Path(tmp) / "db")
        for sql, params in SEED:
            store.db.execute(sql, params)
        runtime = SimpleNamespace(store=store, clock=SimpleNamespace(now=lambda: 30.))
        actor = ThreadActor(runtime, SESSION)
        steps = []
        for action in actions:
            store.inbox.add(SESSION, "control", 30., payload={"action": action, "actor": "UOWNER"})
            item = store.inbox.claim(SESSION)
            await actor.on_control(item)
            steps.append({"action": action, "expected": projection(store)})
        store.close()
        return steps


async def main():
    cases = [await capture(actions) for actions in (
        ["restore"], ["close", "restore"], ["archive", "restore"], ["clean", "restore", "clean"]
    )]
    result = {
        "provenance": "Frozen v0.3.11 ThreadActor.on_control, synthetic state, no workers or external I/O",
        "exceptions": [
            "Rust acknowledges the durable control directly instead of queuing a control inbox item",
            "Rust protects owner authority, persists worker stop intents, and blocks restoration during cleanup",
            "Cleaning drops old pending inputs and owner-closes outstanding obligations in v6",
            "Close/clean clear pause_reason in v6; excluded from the legacy state projection",
        ],
        "seed": SEED,
        "cases": cases,
    }
    (ROOT / "tests/corpus/thread_lifecycle.json").write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
    print(f"Captured {sum(map(len, cases))} frozen lifecycle transitions")


if __name__ == "__main__":
    asyncio.run(main())
