"""End-to-end behavior through the real Daemon, with fake Slack, scripted parent calls, and in-memory workers."""

import asyncio
from dataclasses import replace

import pytest

from fridica.core.errors import BackendError
from fridica.core.models import ArtifactRef, MachineState, ThreadKey, WorkerResult

from harness import DeliveryAmbiguous, Harness, RateLimited, action, delegation, peer


def run(harness, body):
    async def scenario():
        await body()
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())


def decide(*responses):
    """A parent script returning ``responses`` in order for decide calls; triage says respond."""
    queue = list(responses)

    def script(kind, data):
        if kind == "triage":
            return {"decision": "respond"}
        if kind == "debrief":
            return {kind: f"{kind} text"}
        return queue.pop(0) if queue else action("ok")
    return script


def test_mention_gets_a_threaded_reply_with_metadata(config, store):
    harness = Harness(config, store, decide(action("Sure, looking now.", summary="Alice wants the CUDA test fixed.")))
    root = None

    async def body():
        nonlocal root
        root = harness.message("<@UOWNER> can you check the CUDA test?")

    run(harness, body)
    post = harness.slack.posts[0]
    assert post["text"] == "Sure, looking now." and post["thread_ts"] == root.ts
    assert (post["meta"].owner, post["meta"].turn, post["meta"].status, post["meta"].kind) == ("UOWNER", 1, "complete", "reply")
    session = store.threads.get(root.key.id)
    assert session.turns == 1 and session.status == "complete" and session.summary.startswith("Alice wants")
    history = store.messages.thread(root.key)
    assert [item.source for item in history] == ["socket", "self"]
    assert store.messages.verdict(root.event_id).startswith("respond")
    data = harness.llm.calls[0][1]
    assert data["trigger"]["message"]["text"].startswith("<@UOWNER>") and data["machines"][0]["name"] == "local"


def test_unaddressed_messages_are_triaged_with_a_channel_cooldown(config, store):
    decisions = iter(["respond", "observe"])

    def script(kind, data):
        return {"decision": next(decisions)} if kind == "triage" else action("I can help with that.")

    harness = Harness(config, store, script)

    async def body():
        harness.message("does anyone know why CUDA init fails?")
        await harness.settle()
        harness.message("another question for the channel")
        await harness.settle()
        harness.message("and a third")

    run(harness, body)
    assert harness.texts() == ["I can help with that."]
    assert [kind for kind, _ in harness.llm.calls] == ["triage", "decide"]  # the third message is inside the cooldown


def test_clarification_loop_pauses_after_three_questions(config, store):
    harness = Harness(config, store, decide(*[action("Which branch?", status="waiting")] * 3))

    async def body():
        root = harness.message("<@UOWNER> fix the build")
        await harness.settle()
        for text in ("main", "the other one", "whatever"):
            harness.message(text, thread=root.ts)
            await harness.settle()

    run(harness, body)
    assert harness.texts() == ["<@UALICE> Which branch?"] * 3
    session = store.threads.list()[0]
    assert session.control == "paused" and "consecutive replies" in session.pause_reason
    assert store.messages.verdict(store.messages.thread(session.key)[-1].event_id).startswith("observe")


def test_local_owner_instruction_reopens_paused_thread_and_delegates_once(config, store):
    harness = Harness(config, store, decide(
        action("Which branch?", status="waiting"),
        action("Running a focused check.", delegate=[delegation("Run focused checks", machine="local", workspace="project")]),
    ))

    async def body():
        root = harness.message("<@UOWNER> check the build")
        await harness.settle()
        await harness.daemon.thread_action(root.key.id, "pause", "UOWNER")
        await harness.settle()
        first = await harness.daemon.instruct_thread(root.key.id, "Use the feature branch and run focused checks.", "instruction-1")
        again = await harness.daemon.instruct_thread(root.key.id, "Use the feature branch and run focused checks.", "instruction-1")
        assert first == again
        await harness.settle()
        assert store.threads.get(root.key.id).control == "active"
        assert store.inbox.instructions(root.key.id)[0]["state"] == "done"
        await harness.daemon.thread_action(root.key.id, "close", "UOWNER")
        await harness.settle()
        await harness.daemon.thread_action(root.key.id, "clean", "UOWNER")
        await harness.settle()
        assert store.inbox.instructions(root.key.id)[0]["text"] == ""

    run(harness, body)
    triggers = [data["trigger"] for kind, data in harness.llm.calls if kind == "decide"]
    assert [item["kind"] for item in triggers].count("owner_instruction") == 1
    assert triggers[1]["text"] == "Use the feature branch and run focused checks."
    assert len(harness.jobs_seen) == 1 and harness.jobs_seen[0]["brief"].endswith("Run focused checks")


def test_peer_agents_and_the_owner_do_not_trigger_replies(config, store):
    harness = Harness(config, store, decide(action("ok"), action("Answering the peer.", status="complete")))

    async def body():
        root = harness.message("<@UOWNER> hello")
        await harness.settle()
        harness.message("I'm done here.", thread=root.ts, sender="UPEER", meta=peer(turn=2))
        harness.message("owner typing by hand", thread=root.ts, sender="UOWNER")
        await harness.settle()
        harness.message("<@UOWNER> what do you think?", thread=root.ts, sender="UPEER", meta=peer(turn=4, status="waiting"))

    run(harness, body)
    assert harness.texts() == ["ok", "Answering the peer."]
    assert harness.slack.posts[1]["meta"].turn == 5  # the peer's turn counter is inherited


def test_finished_discussion_gets_a_channel_debrief(config, store):
    harness = Harness(config, store, decide(action("All done.", discussion="finished")))

    async def body():
        harness.message("<@UOWNER> quick question")

    run(harness, body)
    assert harness.texts() == ["All done.", "Debrief: this discussion is finished.\n\ndebrief text"]
    assert harness.slack.posts[1]["thread_ts"] is None


def test_long_reply_and_details_are_uploaded_after_the_reply(config, store):
    config = replace(config, limits=replace(config.limits, reply_chars=200))
    harness = Harness(config, store, decide(action("word " * 100, details="# Notes")))

    async def body():
        harness.message("<@UOWNER> explain")

    run(harness, body)
    assert len(harness.texts()[0]) <= 200 and "details file" in harness.texts()[0]
    upload = harness.slack.uploads[0]
    assert upload["filename"].startswith("details-") and upload["data"].decode().endswith("# Notes")


def test_parent_failure_blocks_and_a_later_mention_gets_one_notice(config, store):
    harness = Harness(config, store, lambda kind, data: BackendError("claude is down"))

    async def body():
        root = harness.message("<@UOWNER> do the thing")
        await harness.settle()
        harness.message("<@UOWNER> hello?", thread=root.ts)
        await harness.settle()
        harness.message("<@UOWNER> hello??", thread=root.ts)

    run(harness, body)
    texts = harness.texts()
    assert len(texts) == 2 and "look at it myself" in texts[0] and "needs a local look" in texts[1]


def test_observe_only_stores_but_never_posts(config, store):
    harness = Harness(config, store, decide(action("no")), observe_only=True)

    async def body():
        harness.message("<@UOWNER> hi")

    run(harness, body)
    assert harness.slack.posts == [] and harness.llm.calls == []
    assert store.messages.verdict(store.messages.thread(store.threads.list()[0].key)[0].event_id) == "observe: observe-only mode"


# ----- delegation -----

def test_fan_out_to_two_machines_joins_into_one_reply_and_follow_ups_reach_one_worker(config, store):
    def script(kind, data):
        trigger = data.get("trigger", {})
        if trigger.get("kind") == "worker_results":
            machines = sorted(result["machine"] for result in trigger["results"])
            return action(f"Results from {' and '.join(machines)}.")
        text = trigger["message"]["text"]
        if "compare" in text:
            return action("Starting on both machines.", delegate=[
                delegation("Benchmark on snowy.", machine="snowy", workspace="canoe"),
                delegation("Benchmark on dart9.", machine="dart9", workspace="canoe")])
        snowy = next(worker for worker in data["workers"] if worker["machine"] == "snowy")
        return action("Fixing it on snowy.", delegate=[delegation("Fix and rerun.", worker_id=snowy["worker_id"])])

    def work(spec, brief, resume):
        return WorkerResult("partial", f"{spec.machine.name}: 1.8 s/step", report=f"{spec.machine.name} report",
                            machine_state=MachineState(branch="fix/cuda"))

    harness = Harness(config, store, script, work)

    async def body():
        root = harness.message("<@UOWNER> compare this branch on snowy and dart9")
        await harness.settle()
        harness.message("<@UOWNER> can you fix snowy and rerun?", thread=root.ts)

    run(harness, body)
    assert harness.texts() == ["Starting on both machines.", "Results from dart9 and snowy.", "Fixing it on snowy.",
                               "Results from snowy."]
    assert [(job["machine"], job["resume"]) for job in harness.jobs_seen] == [
        ("snowy", ""), ("dart9", ""), ("snowy", harness.jobs_seen[0]["resume"] or "session-" + harness.jobs_seen[0]["worker"])]
    session = store.threads.list()[0]
    workers = store.workers.for_session(session.id)
    assert len(workers) == 2 and session.context.machine in ("snowy", "dart9") and session.status == "complete"
    results_prompt = next(data for kind, data in harness.llm.calls if data.get("trigger", {}).get("kind") == "worker_results")
    assert all("report" in result["result"] for result in results_prompt["trigger"]["results"])
    assert all("report" not in (worker["last_result"] or {}) for worker in harness.llm.calls[-1][1]["workers"])


def test_single_finished_job_posts_its_report_directly_with_artifacts(config, store, workspace):
    (workspace / "worker1").mkdir()  # the worker's slot folder
    (workspace / "worker1" / "plot.png").write_bytes(b"\x89PNG\r\n\x1a\nPIXELS")

    def work(spec, brief, resume):
        return WorkerResult("done", "Plotted it.", report="Here is the plot: 1.2x faster.",
                            artifacts=(ArtifactRef(str(spec.workspace.path / "plot.png"), "png", "speedup"),))

    harness = Harness(config, store, decide(action("Plotting now.", delegate=[delegation("Plot speedup.", workspace="project")])), work)

    async def body():
        harness.message("<@UOWNER> plot the speedup")

    run(harness, body)
    assert harness.texts() == ["Plotting now.", "Here is the plot: 1.2x faster."]
    assert harness.slack.posts[1]["meta"].kind == "report"
    assert harness.slack.uploads[0]["filename"] == "plot.png" and harness.slack.uploads[0]["data"].endswith(b"PIXELS")
    assert [kind for kind, _ in harness.llm.calls] == ["decide"]  # no second parent call on the fast path


def test_failed_jobs_are_explained_by_the_parent(config, store):
    def script(kind, data):
        trigger = data.get("trigger", {})
        if trigger.get("kind") == "worker_results":
            return action("The run failed: " + trigger["results"][0]["error"])
        return action("Starting.", delegate=[delegation("Run it.", machine="dart9")])

    harness = Harness(config, store, script, lambda spec, brief, resume: BackendError("ssh: connection refused"))

    async def body():
        harness.message("<@UOWNER> run the suite on dart9")

    run(harness, body)
    assert harness.texts()[1] == "The run failed: ssh: connection refused"


def test_delegation_is_refused_outside_delegate_channels(config, store):
    config = replace(config, slack=replace(config.slack, delegate_channels=("COTHER",)))
    harness = Harness(config, store, decide(action("Starting.", delegate=[delegation(machine="dart9")]),
                                            action("I can't run jobs from this channel.")))

    async def body():
        harness.message("<@UOWNER> run it")

    run(harness, body)
    assert harness.texts() == ["I can't run jobs from this channel."] and harness.jobs_seen == []


# ----- owner controls, delivery, recovery -----

def test_pause_and_resume_answer_the_latest_message(config, store):
    harness = Harness(config, store, decide(action("first"), action("answered after resume")))

    async def body():
        root = harness.message("<@UOWNER> hi")
        await harness.settle()
        session_id = root.key.id
        await harness.daemon.thread_action(session_id, "pause", "UOWNER")
        await harness.settle()
        harness.message("<@UOWNER> are you there?", thread=root.ts)
        await harness.settle()
        assert harness.texts() == ["first"]
        await harness.daemon.thread_action(session_id, "resume", "UOWNER")

    run(harness, body)
    assert harness.texts() == ["first", "answered after resume"]
    assert [entry["action"] for entry in store.audit.recent()][:2] == ["thread.resume", "thread.pause"]


def test_rate_limits_retry_and_ambiguous_posts_are_never_resent(config, store):
    harness = Harness(config, store, decide(action("hello"), action("second")))

    async def body():
        harness.slack.failures = [RateLimited(0.0)]
        root = harness.message("<@UOWNER> one")
        await harness.settle()
        harness.slack.failures = [DeliveryAmbiguous("internal_error")]
        harness.message("<@UOWNER> two", thread=root.ts)

    run(harness, body)
    assert harness.texts() == ["hello"]
    problems = store.outbox.list()
    assert [item.state for item in problems] == ["ambiguous"]


def test_crash_between_commit_and_send_leaves_an_ambiguous_post(config, store):
    harness = Harness(config, store, decide(action("hello")))

    async def body():
        harness.message("<@UOWNER> one")
        await harness.daemon.threads.idle()
        harness.daemon.threads.sweep()
        await harness.daemon.threads.idle()
        item = store.outbox.ready(harness.daemon.clock.now())[0]
        store.outbox.claim(item.id)  # the daemon dies while sending

    run(harness, body)
    counts = store.recover(harness.daemon.clock.now())
    assert counts["outbox_ambiguous"] == 1 and harness.slack.posts == []


def test_own_post_echo_is_not_processed_again(config, store):
    harness = Harness(config, store, decide(action("hello")))

    async def body():
        root = harness.message("<@UOWNER> one")
        await harness.settle()
        post = harness.slack.posts[0]
        echo = harness.message(post["text"], thread=root.ts, sender="UOWNER", meta=post["meta"], ts=post["ts"])
        assert store.messages.get(echo.event_id) is None

    run(harness, body)
    assert harness.texts() == ["hello"]


def test_linked_messages_reach_the_parent(config, store):
    harness = Harness(config, store, decide(action("Read the spec.")))
    harness.slack.fetched[("COTHER", "123.000456")] = [{"sender": "UBOB", "text": "the spec", "ts": "123.000456"}]

    async def body():
        harness.message("<@UOWNER> see https://team.slack.com/archives/COTHER/p123000456")

    run(harness, body)
    assert harness.llm.calls[0][1]["linked"] == [{"link": "https://team.slack.com/archives/COTHER/p123000456",
                                                  "sender": "UBOB", "text": "the spec"}]


@pytest.mark.parametrize("control", ["close", "clean"])
def test_closing_or_cleaning_a_thread_stops_its_workers(config, store, control):
    harness = Harness(config, store, decide(action("Starting.", delegate=[delegation(machine="dart9")])))

    async def body():
        root = harness.message("<@UOWNER> run it")
        await harness.settle()
        await harness.daemon.thread_action(root.key.id, control, "UOWNER")

    run(harness, body)
    session = store.threads.list()[0]
    assert session.control == ("closed" if control == "close" else "cleaned")
    assert all(worker.status == "stopped" for worker in store.workers.for_session(session.id))
    if control == "clean":
        assert all(item.text == "" for item in store.messages.thread(session.key))


# ----- regressions from review -----

def test_a_committed_item_is_never_rerun_after_a_later_failure(config, store):
    from fridica.store import StaleSession
    from fridica.threads.actor import ThreadActor
    harness = Harness(config, store, decide(action("hello")))

    async def body():
        root = harness.message("<@UOWNER> one")
        actor = ThreadActor(harness.daemon, root.key.id)
        original = actor.handle

        async def handle_then_fail(item):
            await original(item)
            raise StaleSession(root.key.id)  # something after the commit went stale

        actor.handle = handle_then_fail
        await actor.run()

    run(harness, body)
    assert [kind for kind, _ in harness.llm.calls] == ["decide"] and harness.texts() == ["hello"]


def test_debrief_is_its_own_step_and_follows_the_reply(config, store):
    harness = Harness(config, store, decide(action("All done.", discussion="finished")))

    async def body():
        harness.message("<@UOWNER> quick question")

    run(harness, body)
    debrief = store.outbox.list(states=("sent",))
    assert [item.kind for item in sorted(debrief, key=lambda item: item.id)] == ["reply", "debrief_root"]
    assert sorted(debrief, key=lambda item: item.id)[1].after.endswith(":reply")
    assert [kind for kind, _ in harness.llm.calls] == ["decide", "debrief"]


def test_observe_only_never_posts_leftovers_or_reports_results(config, store):
    from fridica.core.models import Job, OutboxItem, WorkerRecord
    session = store.threads.ensure(ThreadKey("TTEAM", "CROOM", "100.000001"), 1.0)
    store.outbox.enqueue(OutboxItem("left", session.id, "reply", "CROOM", "100.000001", "leftover"), 1.0)
    store.workers.add(WorkerRecord("w1", session.id, "dart9", "canoe", "codex"), 1.0)
    store.jobs.add(Job("j1", "w1", session.id, "brief"), 1.0)
    store.jobs.start("j1", 1.0)
    harness = Harness(config, store, decide(action("no")), observe_only=True)

    async def body():
        harness.daemon.recover()
        task = asyncio.create_task(harness.daemon.run(control=False))
        await asyncio.sleep(0.2)
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)

    asyncio.run(body())
    assert harness.slack.posts == [] and harness.llm.calls == [] and harness.jobs_seen == []


def test_interrupted_jobs_are_reported_or_resumed(config, store):
    harness = Harness(config, store, decide(action("Starting.", delegate=[delegation("Long run.", machine="dart9")])))

    async def start_job():
        harness.message("<@UOWNER> run it")
        await harness.daemon.threads.idle()
        harness.daemon.threads.sweep()
        await harness.daemon.threads.idle()
        await harness.daemon.dispatcher.drain()

    asyncio.run(start_job())
    job = store.jobs.queued()[0]
    store.jobs.start(job.id, 1.0)  # the daemon dies while the job runs
    store.recover(2.0)
    harness2 = Harness(config, store, decide())
    harness2.counter = 100

    async def report():
        await harness2.settle()

    asyncio.run(report())
    assert harness2.texts() == ["One of the jobs I started for this thread was interrupted by a restart and was not resumed."]

    resumed_config = replace(config, limits=replace(config.limits, auto_resume=True))
    harness3 = Harness(resumed_config, store, decide(action("Again.", delegate=[delegation("Long run.", worker_id=job.worker_id)])))
    harness3.counter = 200

    async def again():
        root = store.threads.list()[0]
        harness3.message("<@UOWNER> rerun", thread=root.key.root_ts)
        await harness3.daemon.threads.idle()
        harness3.daemon.threads.sweep()
        await harness3.daemon.threads.idle()
        second = [item for item in store.jobs.queued()][0]
        store.jobs.start(second.id, 3.0)
        store.recover(4.0)
        await harness3.settle()
        return second

    second = asyncio.run(again())
    rerun = [item for item in store.jobs.for_worker(job.worker_id) if item.id not in (job.id, second.id)]
    assert len(rerun) == 1 and rerun[0].brief == second.brief and rerun[0].status == "done"


def test_reload_rebuilds_the_parent_limiter(config, store):
    harness = Harness(config, store, decide())
    old = harness.daemon.parent_slots
    harness.daemon.reload(replace(config, limits=replace(config.limits, parent_concurrency=1)))
    assert harness.daemon.parent_slots is not old and harness.daemon.config.limits.parent_concurrency == 1


def test_long_threads_have_no_turn_limit(config, store):
    harness = Harness(config, store, decide(*[action(f"reply {index}") for index in range(12)]))

    async def body():
        root = harness.message("<@UOWNER> start")
        await harness.settle()
        for index in range(11):
            harness.message(f"<@UOWNER> step {index}", thread=root.ts)
            await harness.settle()

    run(harness, body)
    assert harness.texts() == [f"reply {index}" for index in range(12)]
    assert store.threads.list()[0].turns == 12 and store.threads.list()[0].control == "active"


def test_failing_items_are_retried_a_few_times_then_dropped(config, store, monkeypatch):
    from fridica.threads import actor as actor_module
    harness = Harness(config, store, decide(action("finally")))
    failures = {"left": 2}
    original = actor_module.ThreadActor.on_message

    async def flaky(self, item):
        if failures["left"]:
            failures["left"] -= 1
            raise RuntimeError("database is locked")
        await original(self, item)

    monkeypatch.setattr(actor_module.ThreadActor, "on_message", flaky)

    async def body():
        harness.message("<@UOWNER> one")

    run(harness, body)
    assert harness.texts() == ["finally"]

    failures["left"] = 99
    harness2 = Harness(config, store, decide())
    harness2.counter = 50

    async def body2():
        harness2.message("<@UOWNER> two")

    run(harness2, body2)
    assert harness2.texts() == [] and store.audit.recent()[0]["action"] == "inbox.dropped"
