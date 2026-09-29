"""Capture complete synthetic boundaries for a frozen daemon delegation flow.

Run with PYTHONPATH=spec. No network, model or worker subprocess is used. The
fixture retains parent requests/responses, worker calls/completions and deliveries
in observed order. Rust compares the declared state projection; parent envelopes,
identifier formats and new attention calls are not claimed to be identical.
"""
import asyncio
from dataclasses import asdict
import json
from pathlib import Path
import sys
import tempfile
import uuid
from unittest.mock import patch

from fridica.config import load_config
from fridica.core.models import Message, WorkerResult
from fridica.store import Store

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "spec/tests"))
from harness import Harness, action, delegation  # noqa: E402


def projection(store):
    value = {
        "jobs": [dict(r) for r in store.db.all("SELECT brief,status,attempt,reported,join_group,deliverable,fetch_repo,fetch_ref FROM jobs ORDER BY rowid")],
        "workers": [dict(r) for r in store.db.all("SELECT machine,workspace,backend,role,ephemeral,status,slot FROM workers ORDER BY rowid")],
        "inbox": [dict(r) for r in store.db.all("SELECT kind,state FROM thread_inbox ORDER BY id")],
        "outbox": [dict(r) for r in store.db.all("SELECT kind,text,state,attempts FROM outbox ORDER BY id")],
        "threads": [dict(r) for r in store.db.all("SELECT status,control,turns,wait_streak,no_progress,context_json FROM threads ORDER BY id")],
        "verdicts": [r["verdict"].split(":", 1)[0] for r in store.db.all("SELECT verdict FROM messages WHERE source!='self' ORDER BY id")],
    }
    for thread in value["threads"]:
        thread["context"] = json.loads(thread.pop("context_json"))
    return value


async def capture(grouped=False, observe=False):
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "project").mkdir()
        source = f'''[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local]
backends=["codex"]
max_jobs=2
max_workers=2
[machines.local.workspaces]
project="{root / 'project'}"
[state]
path="{root / 'db'}"
'''
        (root / "config.toml").write_text(source)
        config = load_config(root / "config.toml")
        store = Store(root / "db")
        events = []
        response = action("Running checks.", delegate=[delegation("Run focused checks", machine="local", workspace="project")])
        if grouped:
            response["delegate"].append(delegation("Independent review", role="reviewer", ephemeral=True))
        responses = [response, action("Both checks passed.")]
        def script(kind, data):
            result = responses.pop(0)
            events.append({"kind": "parent", "request": {"kind": kind, "data": data}, "response": result})
            return result
        result = WorkerResult("done", "Checks passed", report="Checks passed.")
        def work(spec, brief, resume):
            events.append({"kind": "worker", "request": {"worker_id": spec.worker_id, "brief": brief, "resume": resume}, "response": asdict(result)})
            return result
        harness = Harness(config, store, script, work, observe_only=observe)
        harness.counter = 100000
        # Record the full protocol boundary as well as the parsed parent context.
        original = harness.llm.call
        async def call(prompt, schema, *, model=""):
            events.append({"kind": "parent_call", "prompt": prompt, "schema": schema, "model": model})
            return await original(prompt, schema, model=model)
        harness.llm.call = call
        post = harness.slack.post
        async def send(channel, text, *, thread_ts, meta):
            response = await post(channel, text, thread_ts=thread_ts, meta=meta)
            events.append({"kind": "delivery", "request": {"channel": channel, "text": text, "thread_ts": thread_ts, "meta": asdict(meta)}, "response": response})
            return response
        harness.slack.post = send
        message = Message("e1", "TTEAM", "CROOM", "100.1", None, "UALICE", "<@UOWNER> run checks")
        events.append({"kind": "intake", "message": asdict(message)})
        harness.daemon.receive(message)
        await harness.settle()
        expected = projection(store)
        await harness.daemon.close()
        store.close()
        return json.loads(json.dumps({"name": "observe" if observe else "grouped" if grouped else "single", "events": events, "expected": expected, "rust_attention": {"grouped_answers_original_mention": grouped}}).replace(temporary, "__ROOT__"))


async def main():
    counter = 0
    def identifier():
        nonlocal counter
        counter += 1
        return uuid.UUID(hex=f"{counter:08x}" + "0" * 24)
    fixtures = []
    for grouped, observe in [(False, False), (True, False), (False, True)]:
        counter = 0
        with patch("uuid.uuid4", identifier):
            fixtures.append(await capture(grouped, observe))
    (ROOT / "tests/corpus/flow.json").write_text(json.dumps({
        "source": "v0.3.11 synthetic daemon capture",
        "scope": "Final verdict classes, inbox states, outbox content/order, jobs, workers and thread transitions; complete captured adapter boundaries retained for inspection. Rust parent envelopes and IDs differ; attention responses are scripted separately.",
        "fixtures": fixtures,
    }, sort_keys=True, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    asyncio.run(main())
