"""F5: an identical reply is not resent in the same thread, except for an @-mention, a repost request, or a
correction."""

import asyncio
import json

from fridica.core.models import WorkerResult
from harness import DeliveryRejected, Harness, action


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
    for text in ("don't repeat that", "thanks, noted", "we should repeat the benchmark tomorrow",
                 "don’t repost", "don't ever repost", "we should repeat that run"):
        assert not asks(text), text


def test_an_owner_resume_gets_the_reply_again(config, store):
    """Reviewer finding: resume replays the latest message; its answer must be posted even if it repeats."""
    harness = Harness(config, store, script(action("CI is green."), action("CI is green.")))

    async def scenario():
        root = harness.message("<@UOWNER> CI?")
        await harness.settle()
        session = store.threads.list()[0].id
        await harness.daemon.thread_action(session, "pause", "owner")
        await harness.settle()
        harness.message("and it is still green", thread=root.ts)  # same asker, no question: observed while paused
        await harness.settle()
        await harness.daemon.thread_action(session, "resume", "owner")
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    assert harness.texts() == ["CI is green.", "CI is green."]


def test_someone_else_or_a_question_gets_the_same_answer_again(config, store):
    """Reviewer finding: only a follow-up from the same person that asks nothing new is a pure repeat."""
    same = "CI is green on main."
    harness = Harness(config, store, script(*[action(same)] * 4))

    async def scenario():
        root = harness.message("<@UOWNER> is CI green on main?")
        await harness.settle()
        harness.message("is CI still green on main?", thread=root.ts, sender="UBOB")  # someone else asks
        await harness.settle()
        harness.message("still green on main?", thread=root.ts, sender="UBOB")  # the same asker, but a question
        await harness.settle()
        harness.message("ok noted", thread=root.ts, sender="UBOB")  # the same asker, nothing new: dropped
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    assert harness.texts() == [same] * 3


def test_a_reply_that_never_reached_slack_is_sent_again(config, store):
    """Reviewer finding: the hash is recorded when a reply is queued, so a failed post must not block the retry."""
    harness = Harness(config, store, script(action("Here is the answer."), action("Here is the answer.")))
    harness.slack.failures.append(DeliveryRejected("msg_too_long"))
    run(harness, "<@UOWNER> answer please", "hello, did you see my question")
    states = [row["state"] for row in harness.store.db.all("SELECT state FROM outbox WHERE kind='reply' ORDER BY id")]
    assert states == ["failed", "sent"] and harness.texts() == ["Here is the answer."]
