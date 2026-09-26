"""F5: an identical reply is not resent in the same thread, except for an @-mention, a repost request, or a
correction."""

import asyncio
import json

from fridica.core.models import WorkerResult
from harness import Harness, action


def script(*responses):
    queue = list(responses)

    def respond(kind, data):
        if kind == "triage":
            return {"decision": "respond"}
        return queue.pop(0) if queue else action("ok")
    return respond


def run(harness, root_text, *follow_ups):
    async def scenario():
        root = harness.message(root_text)
        await harness.settle()
        for text in follow_ups:
            harness.message(text, thread=root.ts)
            await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())


def replies(harness):
    return [row for row in harness.store.db.all("SELECT kind FROM outbox") if row["kind"] == "reply"]


def test_two_identical_decisions_leave_one_outbox_row(config, store):
    """The named F5 test."""
    harness = Harness(config, store, script(action("SIGN-OFF #218 @ 924d2d8"), action("SIGN-OFF #218 @ 924d2d8")))
    run(harness, "<@UOWNER> sign off #218?", "thanks, noted")
    assert len(replies(harness)) == 1 and harness.texts() == ["SIGN-OFF #218 @ 924d2d8"]
    turns = harness.store.db.all("SELECT action_json FROM parent_turns WHERE call='decide' ORDER BY id")
    assert json.loads(turns[-1]["action_json"])["reply"]["suppressed_repeat"] is True


def test_an_at_mention_or_repost_request_still_gets_the_same_text(config, store):
    same = [action("SIGN-OFF #218 @ 924d2d8")] * 3
    harness = Harness(config, store, script(*same))
    run(harness, "<@UOWNER> sign off #218?", "<@UOWNER> post your sign-off here too", "can you repost that?")
    assert harness.texts() == ["SIGN-OFF #218 @ 924d2d8"] * 3


def test_a_correction_and_different_text_are_never_suppressed(config, store):
    harness = Harness(config, store, script(action("CI is green."), action("CI is green.", kind="correction"),
                                            action("CI is green on 924d2d8.")))
    run(harness, "<@UOWNER> CI?", "that was the old head", "and now?")
    assert harness.texts() == ["CI is green.", "CI is green.", "CI is green on 924d2d8."]


def test_identical_questions_pause_the_thread_instead_of_repeating(config, store):
    harness = Harness(config, store, script(*[action("Which branch?", status="waiting")] * 4))
    run(harness, "<@UOWNER> fix the build", "main", "the other one", "whatever")
    assert harness.texts() == ["<@UALICE> Which branch?"]
    session = store.threads.list()[0]
    assert session.control == "paused" and "without progress" in session.pause_reason


def test_only_a_pure_repeat_is_suppressed(config, store):
    """New details, new jobs, or a status change are progress even when the text repeats."""
    from harness import delegation

    same = "Looking into it."
    harness = Harness(config, store, script(
        action(same), action(same, details="# Log\nnew findings"),
        action(same, delegate=[delegation("Rerun the tests", machine="local", workspace="project")]),
        action(same, status="blocked")))
    run(harness, "<@UOWNER> CI is red", "any update", "and?", "still?")
    assert harness.texts().count(same) == 4


def test_worker_results_always_post(config, store):
    from harness import delegation

    result = "All tests pass."
    work = [delegation("Run the tests", machine="local", workspace="project")]
    harness = Harness(config, store, script(action("Running.", delegate=work), action(result),
                                            action("Rerunning.", delegate=work), action(result)))
    harness.work = lambda spec, brief, resume: WorkerResult("partial", "ran", report="")
    run(harness, "<@UOWNER> run the tests", "<@UOWNER> run them again")
    assert harness.texts().count(result) == 2


def test_repost_requests_are_recognised_narrowly():
    from fridica.core.models import Message
    from fridica.threads.policy import repost_requested

    def asks(text):
        return repost_requested(Message("e", "T", "C", "1.0", None, "UA", text), "UOWNER")

    for text in ("can you repost that?", "post your sign-off again", "one more time please", "paste it again"):
        assert asks(text), text
    for text in ("don't repeat that", "thanks, noted", "we should repeat the benchmark tomorrow"):
        assert not asks(text), text
