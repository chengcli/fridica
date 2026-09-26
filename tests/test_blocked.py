"""F4: a blocked thread answers a later mention once with its blocker and next step, naming nobody with a mention."""

import asyncio

from harness import Harness, action


def blocked(blocker="", assignee="", next_step=""):
    reply = action("I can't build this here.", status="blocked")
    reply["note"].update(blocker=blocker, assignee=assignee, next_step=next_step)
    return reply


def run(harness, root_text, *follow_ups):
    async def scenario():
        root = harness.message(root_text)
        await harness.settle()
        for text in follow_ups:
            harness.message(text, thread=root.ts)
            await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())


def script(*responses):
    queue = list(responses)
    return lambda kind, data: queue.pop(0) if queue else action("unexpected")


def test_blocked_notice_carries_the_blocker_from_the_note(config, store):
    """The named F4 test: the note's blocker reaches the outbox text."""
    harness = Harness(config, store, script(blocked("no CUDA build on this machine", "USIHE", "build kintera with CUDA")))
    harness.slack.names["USIHE"] = "chen sihe"
    run(harness, "<@UOWNER> run the GPU test", "<@UOWNER> any news?")
    assert harness.texts()[-1] == "Blocked: no CUDA build on this machine. Next: chen sihe to build kintera with CUDA."
    assert "no CUDA build on this machine" in harness.store.outbox.for_session(store.threads.list()[0].id)[-1].text


def test_the_notice_is_posted_once_and_pings_nobody(config, store):
    note = blocked("waiting for a <@UXI> decision on <!channel>", "UXI", "to pick the merge window.")
    harness = Harness(config, store, script(note))
    harness.slack.names["UXI"] = "Xi Zhang"
    run(harness, "<@UOWNER> merge #218", "<@UOWNER> ping", "<@UOWNER> ping again")
    notices = [text for text in harness.texts() if text.startswith("Blocked:")]
    assert notices == ["Blocked: waiting for a Xi Zhang decision on channel. Next: Xi Zhang to pick the merge window."]
    assert all("<@" not in text and "<!" not in text for text in harness.texts())
    assert len(harness.llm.calls) == 1  # a blocked thread does not call the parent again


def test_partial_and_empty_notes(config, store):
    harness = Harness(config, store, script(blocked(next_step="restart the listener")))
    run(harness, "<@UOWNER> deploy", "<@UOWNER> status?")
    assert harness.texts()[-1] == ("Blocked: this needs someone with local access before I can continue. "
                                   "Next: restart the listener.")


def test_slack_client_looks_up_names_once_and_falls_back_to_the_id(config):
    from fridica.slack.egress import SlackClient

    class Web:
        def __init__(self):
            self.calls = []

        async def users_info(self, user):
            self.calls.append(user)
            if user == "UGONE":
                raise RuntimeError("user_not_found")
            return {"user": {"name": "sihe", "profile": {"display_name": "", "real_name": "chen sihe"}}}

    web = Web()
    client = SlackClient(config, web)
    assert asyncio.run(client.user_name("USIHE")) == "chen sihe"
    assert asyncio.run(client.user_name("USIHE")) == "chen sihe" and web.calls == ["USIHE"]
    assert asyncio.run(client.user_name("UGONE")) == "UGONE"


def test_a_failed_name_lookup_does_not_cause_a_second_notice(config, store):
    harness = Harness(config, store, script(blocked("no GPU", "USIHE", "run it on Node 3")))
    answers = iter(["USIHE", "chen sihe", "chen sihe"])

    async def flaky_name(user):
        return next(answers)

    harness.slack.user_name = flaky_name
    run(harness, "<@UOWNER> GPU test", "<@UOWNER> news?", "<@UOWNER> news??")
    assert [text for text in harness.texts() if text.startswith("Blocked:")] == [
        "Blocked: no GPU. Next: USIHE to run it on Node 3."]


def test_an_old_blocker_is_not_repeated_after_a_resume(config, store):
    from fridica.core.errors import BackendError

    responses = [blocked("no CUDA build", "USIHE", "build with CUDA"), BackendError("claude is down")]
    harness = Harness(config, store, lambda kind, data: responses.pop(0) if responses else action("x"))

    async def scenario():
        root = harness.message("<@UOWNER> GPU test")
        await harness.settle()
        await harness.daemon.thread_action(store.threads.list()[0].id, "resume", "owner")
        await harness.settle()
        harness.message("<@UOWNER> try again", thread=root.ts)  # the parent fails: blocked, with no note
        await harness.settle()
        harness.message("<@UOWNER> status?", thread=root.ts)
        await harness.settle()
        await harness.daemon.close()
    asyncio.run(scenario())
    notice = harness.texts()[-1]
    assert notice.startswith("Blocked: this needs someone with local access") and "CUDA" not in notice


def test_group_mentions_are_defused_and_fields_are_capped(config, store):
    harness = Harness(config, store, script(blocked("waiting on <!subteam^S123|@kintera-devs> " + "x" * 2000,
                                                    "", "decide")))
    run(harness, "<@UOWNER> merge", "<@UOWNER> ping")
    notice = harness.texts()[-1]
    assert "<!" not in notice and "kintera-devs" in notice and len(notice) < 700
