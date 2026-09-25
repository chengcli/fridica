from dataclasses import replace

import pytest

from fridica.core.models import (
    Approval, FridicaMeta, Job, OutboxItem, StickyContext, ThreadKey, WorkerRecord, WorkerResult,
)
from fridica.store import StaleSession


def test_intake_creates_thread_and_inbox_in_one_step(store, message):
    root = message(ts="100.000001")
    session_id, inbox_id = store.messages.intake(root, 1.0)
    assert session_id == "TTEAM:CROOM:100.000001" and inbox_id is not None
    reply = message(ts="100.000002", thread_ts="100.000001")
    assert store.messages.intake(reply, 2.0)[0] == session_id
    session = store.threads.get(session_id)
    assert session.key == ThreadKey("TTEAM", "CROOM", "100.000001") and session.status == "new"
    assert [item.ref for item in store.inbox.pending(session_id)] == [root.event_id, reply.event_id]
    assert [item.ts for item in store.messages.thread(session.key)] == ["100.000001", "100.000002"]


def test_duplicates_by_channel_and_ts_are_ignored(store, message):
    first = message(ts="100.000001")
    store.messages.intake(first, 1.0)
    again = replace(first, event_id="catchup:CROOM:100.000001", source="catchup")
    assert store.messages.intake(again, 2.0)[1] is None
    assert len(store.inbox.pending(first.key.id)) == 1


def test_own_posts_are_history_but_not_work(store, message):
    own = message(ts="100.000005", thread_ts="100.000001", sender="UOWNER", source="self",
                  meta=FridicaMeta(owner="UOWNER", turn=1))
    session_id, inbox_id = store.messages.intake(own, 1.0)
    assert inbox_id is None and store.inbox.pending(session_id) == []
    assert store.messages.thread(own.key)[0].meta.owner == "UOWNER"


def test_inbox_claim_is_ordered_and_exclusive(store, message):
    for index in range(3):
        store.messages.intake(message(ts=f"100.00000{index + 1}", thread_ts="100.000001"), 1.0)
    session_id = "TTEAM:CROOM:100.000001"
    first = store.inbox.claim(session_id)
    second = store.inbox.claim(session_id)
    assert first.id < second.id
    store.inbox.finish(first.id)
    assert [item.id for item in store.inbox.pending(session_id)] == [second.id + 1]
    assert store.inbox.pending_sessions() == [session_id]


def test_session_save_is_optimistic(store, message):
    session_id, _ = store.messages.intake(message(ts="100.000001"), 1.0)
    session = store.threads.get(session_id)
    saved = store.threads.save(replace(session, status="waiting", context=StickyContext(machine="snowy")), 2.0)
    assert saved.version == 1 and store.threads.get(session_id).context.machine == "snowy"
    with pytest.raises(StaleSession):
        store.threads.save(replace(session, status="complete"), 3.0)


def test_channel_recent_and_latest_unanswered(store, message):
    store.messages.intake(message("earlier", ts="90.000001"), 1.0)
    store.messages.intake(message("root", ts="100.000001"), 1.0)
    store.messages.intake(message("mine", ts="100.000002", thread_ts="100.000001", sender="UOWNER", source="self",
                                  meta=FridicaMeta(owner="UOWNER")), 1.0)
    store.messages.intake(message("again?", ts="100.000003", thread_ts="100.000001"), 1.0)
    key = ThreadKey("TTEAM", "CROOM", "100.000001")
    assert [item.text for item in store.messages.channel_recent("TTEAM", "CROOM", "100.000001")] == ["earlier"]
    assert store.messages.latest_unanswered(key).text == "again?"


def test_workers_jobs_and_results_round_trip(store, message):
    session_id, _ = store.messages.intake(message(ts="100.000001"), 1.0)
    worker = store.workers.add(WorkerRecord("w1", session_id, "snowy", "exocubed", "codex"), 1.0)
    assert worker.status == "idle"
    job = store.jobs.add(Job("j1", "w1", session_id, "fix it", join_group="g1"), 1.0)
    assert store.jobs.queued() == [job] and store.jobs.start("j1", 2.0) and not store.jobs.start("j1", 2.0)
    result = WorkerResult("done", "Fixed CUDA init.", report="Fixed it.")
    store.jobs.finish("j1", "done", 3.0, result=result)
    store.workers.record_result("w1", result, "thread-9", "idle", 3.0)
    assert store.jobs.get("j1").result == result
    worker = store.workers.get("w1")
    assert worker.backend_session_id == "thread-9" and worker.summary == "Fixed CUDA init."
    store.workers.record_result("w1", None, "", "idle", 4.0)
    assert store.workers.get("w1").backend_session_id == "thread-9"


def test_outbox_is_idempotent_ordered_and_respects_prerequisites(store):
    reply = OutboxItem("i1:reply:0", "s", "reply", "CROOM", "100.1", "hello")
    upload = OutboxItem("i1:upload:0", "s", "upload", "CROOM", "100.1", filename="d.md", blob=b"x", after="i1:reply:0")
    other = OutboxItem("i2:reply:0", "s2", "reply", "CROOM", "200.1", "other thread")
    assert store.outbox.enqueue(reply, 1.0) and not store.outbox.enqueue(reply, 1.0)
    store.outbox.enqueue(upload, 1.0)
    store.outbox.enqueue(other, 1.0)
    assert [item.idem_key for item in store.outbox.ready(1.0)] == ["i1:reply:0", "i2:reply:0"]
    first = store.outbox.get("i1:reply:0")
    store.outbox.claim(first.id)
    store.outbox.sent(first.id, "100.2")
    assert [item.idem_key for item in store.outbox.ready(1.0)] == ["i1:upload:0", "i2:reply:0"]


def test_recovery_settles_in_flight_work(store, message):
    session_id, inbox_id = store.messages.intake(message(ts="100.000001"), 1.0)
    store.inbox.claim(session_id)
    store.workers.add(WorkerRecord("w1", session_id, "snowy", "exocubed", "codex", status="running"), 1.0)
    store.jobs.add(Job("j1", "w1", session_id, "brief"), 1.0)
    store.jobs.start("j1", 1.0)
    store.outbox.enqueue(OutboxItem("k", session_id, "reply", "CROOM", "100.000001", "hi"), 1.0)
    store.outbox.claim(store.outbox.get("k").id)
    store.approvals.add(Approval("a1", "w1", "j1", session_id, "command", "run make", created=1.0))
    counts = store.recover(5.0)
    assert counts == {"inbox_done": 0, "inbox_retried": 1, "outbox_ambiguous": 1, "jobs_interrupted": 1,
                      "approvals_expired": 1}
    assert store.jobs.get("j1").status == "interrupted" and store.workers.get("w1").status == "lost"
    assert store.outbox.get("k").state == "ambiguous"
    assert [item.kind for item in store.inbox.pending(session_id)] == ["message", "worker_interrupted"]
    assert store.approvals.get("a1").status == "expired"


def test_notes_are_revisioned(store):
    assert store.notes.current("s") == (0, {})
    assert store.notes.write("s", {"goal": "x"}, "UOWNER", 1.0) == 1
    with pytest.raises(ValueError, match="notes changed"):
        store.notes.write("s", {"goal": "y"}, "UOWNER", 1.0, expected=0)
    assert store.notes.current("s") == (1, {"goal": "x"})


def test_failed_prerequisites_block_dependents_visibly_and_retry_unblocks(store):
    store.outbox.enqueue(OutboxItem("r", "s", "reply", "CROOM", "1.0", "hi"), 1.0)
    store.outbox.enqueue(OutboxItem("u", "s", "upload", "CROOM", "1.0", filename="a.md", blob=b"x", after="r"), 1.0)
    store.outbox.enqueue(OutboxItem("u2", "s", "upload", "CROOM", "1.0", filename="b.md", blob=b"x", after="u"), 1.0)
    store.outbox.enqueue(OutboxItem("later", "s", "notice", "CROOM", "1.0", "later"), 1.0)
    reply = store.outbox.get("r")
    store.outbox.claim(reply.id)
    store.outbox.fail(reply.id, "ambiguous", "5xx")
    assert [store.outbox.get(key).state for key in ("u", "u2")] == ["blocked", "blocked"]
    assert [item.idem_key for item in store.outbox.ready(1.0)] == ["later"]
    assert {item.idem_key for item in store.outbox.list()} == {"r", "u", "u2"}
    assert not store.outbox.requeue(store.outbox.get("u").id)
    assert store.outbox.requeue(reply.id)
    assert [store.outbox.get(key).state for key in ("r", "u", "u2")] == ["pending"] * 3
    with pytest.raises(ValueError, match="not queued"):
        store.outbox.enqueue(OutboxItem("x", "s", "upload", "CROOM", "1.0", after="missing"), 1.0)


def test_recovery_treats_any_parent_turn_as_handled(store, message):
    session_id, inbox_id = store.messages.intake(message(ts="100.000001"), 1.0)
    store.inbox.claim(session_id)
    store.db.execute("INSERT INTO parent_turns (session_id, inbox_id, backend, call, created) VALUES (?,?,?,?,?)",
                     (session_id, inbox_id, "claude", "decide", 1.0))
    assert store.recover(2.0)["inbox_done"] == 1
    assert store.inbox.get(inbox_id).state == "done"
