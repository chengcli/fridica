"""Capture deterministic delivery fixtures from v0.3.11; run with PYTHONPATH=spec.

The fixture scopes parity to outbox state, confirmed self-history and delivery
order. Durable attempt fencing and attention records have separate Rust tests.
"""
import asyncio
from dataclasses import asdict
import json
from pathlib import Path
import tempfile

from fridica.core.bus import Bus
from fridica.core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from fridica.core.models import FridicaMeta, OutboxItem
from fridica.slack.outbox import OutboxDispatcher
from fridica.store import Store


class Clock:
    value = 1.0

    def now(self):
        return self.value


class Delivery:
    def __init__(self, outcomes):
        self.outcomes = list(outcomes)
        self.sent = 0
        self.uploaded = 0
        self.calls = []

    async def post(self, channel, text, *, thread_ts, meta):
        return self._send({"kind": "post", "channel": channel, "text": text,
                           "thread_ts": thread_ts, "meta": asdict(meta) if meta else None})

    async def upload(self, channel, thread_ts, data, filename):
        return self._send({"kind": "upload", "channel": channel, "thread_ts": thread_ts,
                           "blob": list(data), "filename": filename})

    def _send(self, call):
        self.calls.append(call)
        result = self.outcomes.pop(0) if self.outcomes else {"outcome": "sent"}
        if result["outcome"] == "ambiguous":
            raise DeliveryAmbiguous(result["code"])
        if result["outcome"] == "rejected":
            raise DeliveryRejected(result["code"])
        if result["outcome"] == "rate_limited":
            raise RateLimited(result["retry_after"])
        if call["kind"] == "upload":
            self.uploaded += 1
            return f"F{self.uploaded}"
        self.sent += 1
        return f"200.{self.sent:06d}"


def post(key, thread="100.1", after="", upload=False):
    return OutboxItem(key, f"TTEAM:CROOM:{thread}", "upload" if upload else "reply", "CROOM", thread,
                      text="" if upload else key, after=after,
                      meta=None if upload else FridicaMeta("UOWNER", session=f"TTEAM:CROOM:{thread}", turn=1, status="complete"),
                      filename="result.txt" if upload else "", blob=b"result" if upload else None)


def projection(store, calls):
    return {
        "outbox": [dict(row) for row in store.db.all(
            "SELECT idem_key,state,attempts,retry_at,sent_ts FROM outbox ORDER BY id")],
        "history": [{**dict(row), "meta": json.loads(row["meta"]) if row["meta"] else None} for row in store.db.all(
            "SELECT workspace,channel,ts,root_ts,thread_ts,sender,text,source,meta_json AS meta FROM messages ORDER BY id")],
        "calls": list(calls),
    }


async def capture(name, posts, outcomes, ticks):
    with tempfile.TemporaryDirectory() as directory:
        store = Store(Path(directory) / "state.sqlite3")
        clock, delivery = Clock(), Delivery(outcomes)
        dispatcher = OutboxDispatcher(store, delivery, Bus(), owner="UOWNER", clock=clock)
        for item in posts:
            store.outbox.enqueue(item, clock.now())
        snapshots = []
        for at in ticks:
            clock.value = at
            sent = await dispatcher.drain()
            snapshots.append({"at": at, "sent": sent, "expected": projection(store, delivery.calls)})
        inputs = []
        for item in posts:
            data = asdict(item)
            inputs.append({key: data[key] for key in (
                "idem_key", "session_id", "kind", "channel", "thread_ts", "text", "meta", "filename", "after")}
                          | {"blob": list(item.blob) if item.blob is not None else None})
        store.close()
        return {"name": name, "posts": inputs, "outcomes": outcomes, "snapshots": snapshots}


async def main():
    fixtures = [
        await capture("ordered_reply_upload", [post("a"), post("file", after="a", upload=True), post("b", "100.2")], [], [2., 3.]),
        await capture("rate_limit_independent_thread", [post("a"), post("file", after="a", upload=True), post("b", "100.2")],
                      [{"outcome": "rate_limited", "retry_after": 15.}], [2., 16., 17., 18.]),
        await capture("ambiguous_with_dependent", [post("a"), post("file", after="a", upload=True), post("independent")],
                      [{"outcome": "ambiguous", "code": "disconnected"}], [2., 3., 4.]),
        await capture("rejected_with_independent_thread", [post("a"), post("file", after="a", upload=True), post("b", "100.2")],
                      [{"outcome": "rejected", "code": "missing_scope"}], [2., 3.]),
        await capture("exhausted_rate_limit", [post("a")], [{"outcome": "rate_limited", "retry_after": 1.}] * 5, [1., 2., 3., 4., 5., 10.]),
        await capture("duplicate_enqueue", [post("a"), post("a")], [], [2.]),
    ]
    output = {"baseline": "v0.3.11", "scope": ["outbox", "history", "calls"], "fixtures": fixtures}
    path = Path(__file__).resolve().parents[1] / "tests/corpus/outbox.json"
    path.write_text(json.dumps(output, indent=2) + "\n")


if __name__ == "__main__":
    asyncio.run(main())
