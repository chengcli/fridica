"""The design document's statistics scripts keep working as the schema evolves, and never leak Slack IDs."""

import json
from pathlib import Path
import sys

from fridica.core.models import FridicaMeta, Job, OutboxItem, WorkerRecord

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "docs" / "scripts"))

import common  # noqa: E402
import stats  # noqa: E402


def populate(store, message):
    store.db.bind("UOWNER", "TTEAM")
    root = message(ts="100.000001", sender="UALICE")
    session_id, inbox_id = store.messages.intake(root, 1.0)
    store.messages.intake(message(ts="100.000002", thread_ts="100.000001", sender="UBOBBYBOB"), 2.0)
    store.messages.intake(message(ts="100.000003", thread_ts="100.000001", sender="UPEERPEER",
                                  meta=FridicaMeta(owner="UPEERPEER", turn=1, status="complete")), 3.0)
    store.messages.set_verdict(root.event_id, "respond: addressed")
    store.workers.add(WorkerRecord("w1", session_id, "local", "main", "claude", role="tester"), 4.0)
    store.jobs.add(Job("j1", "w1", session_id, "run the tests", join_group=str(inbox_id), inbox_id=inbox_id), 4.0)
    store.parent_turns.add(session_id, inbox_id, {"call": "decide", "backend": "claude", "prompt_chars": 1234,
                                                  "latency_ms": 2500}, 5.0)
    meta = FridicaMeta(owner="UOWNER", session=session_id, turn=1, status="complete")
    store.outbox.enqueue(OutboxItem(f"{inbox_id}:reply", session_id, "reply", "CROOM", "100.000001", "On it.",
                                    meta=meta), 5.0)
    return session_id


def test_collect_new_counts_a_small_database(store, message, tmp_path):
    populate(store, message)
    with common.snapshot(store.db.path) as db:
        data = stats.collect_new(db)
    assert data["schema_version"] >= 3
    assert data["threads"] == 1 and data["workers"] == 1 and data["jobs"] == 1
    assert data["parent"]["decide"]["count"] == 1 and data["parent_calls"] == 1
    assert data["posts"] == 1 and data["post_kinds"] == {"reply": 1}
    assert data["verdicts"].get("respond") == 1


def test_statistics_are_anonymous(store, message):
    populate(store, message)
    with common.snapshot(store.db.path) as db:
        data = stats.collect_new(db)
    text = json.dumps(data, default=str)
    common.assert_anonymous(text, "collect_new")
    assert "UALICE" not in text and "UBOBBYBOB" not in text and "CROOM" not in text
    assert "On it." not in text  # no message or post text in the statistics


def test_anonymizer_is_stable_and_the_check_catches_ids():
    names = common.Anonymizer(owner="UOWNER1234")
    assert names.person("UOWNER1234") == "owner"
    assert names.person("UALICE12345") == names.person("UALICE12345") == "person A"
    assert names.person("UBOB1234567") == "person B"
    assert names.channel("C0123456789") == "channel 1"
    try:
        common.assert_anonymous("sent by U0123ABCDEF", "test")
    except SystemExit:
        pass
    else:
        raise AssertionError("a Slack ID was not detected")


def test_stats_does_not_need_matplotlib():
    assert "matplotlib" not in Path(stats.__file__).read_text()


def test_machine_names_become_nodes_everywhere():
    new = {"jobs_by_machine_status": {"gpubox": {"done": 2}, "local": {"done": 1}, "?": {"failed": 1}},
           "workers_by_machine_role": {"gpubox": {"tester": 1}}, "peak_by_machine": {"gpubox": 2},
           "slots": {"gpubox:1": 1, "local:-": 1}}
    old = {"heavy_by_host": {"gpubox": 3, "retired": 1}}
    configured = [{"name": "local"}, {"name": "gpubox"}]
    real = stats.anonymize_machines(new, old, configured)
    assert real == ["local", "gpubox", "retired"]
    assert [machine["name"] for machine in configured] == ["Node 1", "Node 2"]
    assert new["jobs_by_machine_status"] == {"Node 2": {"done": 2}, "Node 1": {"done": 1}, "?": {"failed": 1}}
    assert new["slots"] == {"Node 2:1": 1, "Node 1:-": 1}
    assert old["heavy_by_host"] == {"Node 2": 3, "Node 3": 1}
    assert "gpubox" not in json.dumps([new, old, configured])


def test_sandbox_probe_output_is_parsed_strictly():
    import sandbox
    text = "ns_user=private\n`seccomp=2`\nThe output:\nnot a key = value\nwrite_home=blocked\n"
    assert sandbox.parse(text) == {"ns_user": "private", "seccomp": "2", "write_home": "blocked"}


def test_sandbox_probe_runs_on_the_host(tmp_path):
    import pytest
    if not Path("/proc/self/ns/user").exists():
        pytest.skip("needs Linux namespaces")
    import sandbox
    host = sandbox.run("host")
    assert host["ns_user"] == "shared" and host["write_workspace"] == "allowed" and host["signal_host"] == "allowed"
