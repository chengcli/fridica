import asyncio
import json

import pytest

from fridica.core.errors import RateLimited
from fridica.core.ids import SequenceIds
from fridica.core.models import WorkerResult
from fridica.replay import Recorder, Redactor, Replay, compare, load


class Clock:
    def now(self):
        return 1.0


def test_full_response_error_and_completion_order_are_replayable(tmp_path):
    async def run():
        path = tmp_path / "tape.jsonl"
        recorder = Recorder(path, Clock())
        async def slow():
            await asyncio.sleep(0.01)
            return WorkerResult("done", "all tests passed", report="complete report")
        async def fast():
            raise RateLimited(45)
        recorded = await asyncio.gather(recorder.call("worker.run", {"session": "s1"}, slow),
                                        recorder.call("slack.post", {"text": "hello"}, fast), return_exceptions=True)
        recorder.close()
        assert path.stat().st_mode & 0o777 == 0o600
        rows = load(path)
        assert [r["kind"] for r in rows] == ["call", "call", "error", "result"]
        tape = Replay(rows)
        replayed = await asyncio.gather(tape.call("worker.run", {"session": "s1"}),
                                        tape.call("slack.post", {"text": "hello"}), return_exceptions=True)
        assert replayed[0] == recorded[0]
        assert replayed[1].retry_after == 45
        tape.assert_consumed()
    asyncio.run(run())


def test_incomplete_capture_cannot_claim_exact_parity(tmp_path):
    path = tmp_path / "tape.jsonl"
    recorder = Recorder(path, Clock())
    recorder.event("call", {"id": 1, "operation": "slack.post", "arguments": {}})
    recorder.close()
    with pytest.raises(ValueError, match="incomplete"):
        load(path)


def test_parity_exceptions_are_exact_and_cannot_go_stale():
    expected = {"outbox": [{"text": "old"}], "jobs": [], "verdicts": ["respond"]}
    actual = {**expected, "outbox": [{"text": "new"}]}
    with pytest.raises(AssertionError, match="unexplained"):
        compare(expected, actual)
    exception = {"path": "/outbox/0/text", "reason": "scripted attention response in fixture 1"}
    compare(expected, actual, [exception])
    with pytest.raises(AssertionError, match="stale"):
        compare(expected, expected, [exception])


def test_redaction_preserves_thread_relationships_and_removes_unknown_content():
    redactor = Redactor(b"fixture-key-32-bytes-long-enough!!")
    source = [{"workspace": "TTEAM", "channel": "CROOM", "ts": "100.000001", "id": "TTEAM:CROOM:100.000001",
               "text": "secret content xoxp-sensitive", "context": {"private-host": "private/repo"}}]
    result = redactor.table(source, tuple(source[0]))[0]
    assert result["id"] == f"{result['workspace']}:{result['channel']}:{result['ts']}"
    assert "secret" not in json.dumps(result) and "private" not in json.dumps(result)


def test_identifiers_do_not_collide_after_truncation():
    ids = SequenceIds()
    assert len({ids.hex("job", 8) for _ in range(1000)}) == 1000


def test_redaction_keeps_foreign_keys_inside_serialized_json():
    redactor = Redactor(b"fixture-key-32-bytes-long-enough!!")
    rows = [{"event_id": "event-secret", "source_json": '{"event_id":"event-secret"}'}]
    row = redactor.table(rows, ("event_id", "source_json"))[0]
    assert row["source_json"][redactor.pseudonym("event_id")] == row["event_id"]
