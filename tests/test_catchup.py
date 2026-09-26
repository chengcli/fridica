"""F2: catch-up rereads each channel from its newest stored message, capped at seven days, with thread replies."""

import asyncio

import pytest

from fridica.core.bus import Bus
from fridica.core.models import Message
from fridica.slack import catchup

DAY = 86400.0
NOW = 2_000_000_000.0


def payload(ts, text="hello", thread_ts=None, channel="CROOM"):
    event = {"type": "message", "channel": channel, "user": "UALICE", "text": text, "ts": ts}
    if thread_ts:
        event["thread_ts"] = thread_ts
    return {"type": "event_callback", "event_id": f"catchup:{channel}:{ts}", "team_id": "TTEAM", "event": event}


class History:
    """A fake Slack that, like the real one, returns only messages at or after ``oldest``."""

    def __init__(self, messages):
        self.messages = messages
        self.calls: list[tuple[str, float, tuple]] = []

    async def recent(self, channel, oldest, threads=()):
        self.calls.append((channel, oldest, threads))
        return [item for item in self.messages if item["event"]["channel"] == channel
                and float(item["event"]["ts"]) >= oldest]


def stored(store, ts, *, thread_ts=None, now=None):
    message = Message(f"event-{ts}", "TTEAM", "CROOM", ts, thread_ts, "UALICE", "earlier")
    store.messages.intake(message, now if now is not None else float(ts))
    return message


def test_everything_since_the_last_stored_message_is_ingested(config, store):
    """The named F2 test: the store holds a message at t0, the daemon was down five hours, and all N messages after
    t0 arrive; a fixed one-hour window would miss the first four hours."""
    t0 = NOW - 5 * 3600
    stored(store, f"{t0:.6f}")
    missed = [payload(f"{t0 + 600 * index:.6f}", f"missed {index}") for index in range(1, 31)]
    slack = History(missed)
    added = asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW))
    assert added == 30
    assert all(store.messages.exists("TTEAM", "CROOM", item["event"]["ts"]) for item in missed)
    assert slack.calls[0][1] == t0 - catchup.OVERLAP


def test_a_fresh_store_reads_an_hour(config, store):
    slack = History([])
    asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW))
    assert slack.calls[0][1] == NOW - catchup.WINDOW  # nothing stored: do not pull the whole channel


def test_the_window_is_capped_at_seven_days(config, store):
    stored(store, f"{NOW - 30 * DAY:.6f}")
    slack = History([])
    asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW))
    assert dict((channel, start) for channel, start, _ in slack.calls)["CROOM"] == NOW - catchup.MAX_WINDOW


def test_a_live_message_before_the_first_pass_does_not_shrink_the_window(config, store):
    """Socket Mode connects before catch-up runs; a message it stores must not hide the gap before it."""
    started = NOW - 60
    stored(store, f"{NOW - 3 * DAY:.6f}", now=NOW - 3 * DAY)  # before the outage
    stored(store, f"{NOW - 10:.6f}", now=NOW - 10)              # arrived live after this start
    missed = payload(f"{NOW - 2 * DAY:.6f}", "<@UOWNER> during the outage")
    slack = History([missed])
    asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW, started_at=started))
    assert store.messages.exists("TTEAM", "CROOM", missed["event"]["ts"])


def test_a_truncated_pass_does_not_advance_the_watermark(config, store):
    from fridica.slack.egress import IncompleteHistory

    t0 = NOW - 2 * DAY
    stored(store, f"{t0:.6f}")

    class Truncated(History):
        async def recent(self, channel, oldest, threads=()):
            self.calls.append((channel, oldest, threads))
            raise IncompleteHistory([payload(f"{NOW - 60:.6f}", "newest page only", channel=channel)])

    slack = Truncated([])
    with pytest.raises(IncompleteHistory):
        asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW))
    slack.calls.clear()
    with pytest.raises(IncompleteHistory):
        asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.RECENT, NOW + 300))
    assert dict((channel, start) for channel, start, _ in slack.calls)["CROOM"] == t0 - catchup.OVERLAP
    complete = History([])
    asyncio.run(catchup.catch_up(complete, store, config, Bus(), catchup.RECENT, NOW + 600))
    asyncio.run(catchup.catch_up(complete, store, config, Bus(), catchup.RECENT, NOW + 900))
    assert complete.calls[-1][1] == NOW + 900 - catchup.RECENT  # caught up: back to the short window


def test_a_running_daemon_keeps_its_short_window(config, store):
    stored(store, f"{NOW - 30:.6f}")
    slack = History([])
    asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.RECENT, NOW))
    assert slack.calls[0][1] == NOW - catchup.RECENT


def test_replies_are_refetched_for_threads_active_before_the_window(config, store):
    root = f"{NOW - 3 * DAY:.6f}"
    stored(store, root, now=NOW - 2 * DAY - 3600)  # the thread was last active two days ago, then the daemon stopped
    slack = History([payload(f"{NOW - DAY / 2:.6f}", "reply while we were down", thread_ts=root)])
    added = asyncio.run(catchup.catch_up(slack, store, config, Bus(), catchup.WINDOW, NOW))
    assert root in slack.calls[0][2] and added == 1


def test_overlap_is_harmless_and_old_messages_are_history_not_work(config, store):
    t0 = NOW - 3 * DAY
    stored(store, f"{t0:.6f}")
    old = payload(f"{t0 + 3600:.6f}", "two days ago, nobody addressed")
    old_mention = payload(f"{t0 + 7200:.6f}", "<@UOWNER> two days ago, addressed to you")
    new = payload(f"{NOW - 3600:.6f}", "an hour ago")
    slack = History([payload(f"{t0:.6f}", "earlier"), old, old_mention, new])
    rung = []
    bus = Bus()
    bus.on_thread(rung.append)
    assert asyncio.run(catchup.catch_up(slack, store, config, bus, catchup.WINDOW, NOW)) == 2
    assert store.messages.exists("TTEAM", "CROOM", old["event"]["ts"])  # kept as the record of the channel
    assert sorted(rung) == sorted(f"TTEAM:CROOM:{item['event']['ts']}" for item in (old_mention, new))
    assert asyncio.run(catchup.catch_up(slack, store, config, bus, catchup.WINDOW, NOW)) == 0
