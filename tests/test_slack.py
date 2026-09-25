import asyncio

import pytest
from slack_sdk.errors import SlackApiError

from fridica.core.bus import Bus
from fridica.core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from fridica.core.models import FridicaMeta
from fridica.slack import catchup, render
from fridica.slack.egress import SlackClient
from fridica.slack.ingress import dropped_mention, normalize


def payload(**event):
    base = {"type": "message", "channel": "CROOM", "user": "UALICE", "text": "hi", "ts": "100.000001"}
    base.update(event)
    return {"type": "event_callback", "event_id": "Ev1", "team_id": "TTEAM", "event": base}


def test_normalize_top_level_reply_and_rejections():
    top = normalize(payload())
    assert top.thread_ts is None and top.root_ts == "100.000001" and top.meta is None and top.source == "socket"
    reply = normalize(payload(ts="100.000002", thread_ts="100.000001"))
    assert reply.thread_ts == "100.000001" and reply.key.root_ts == "100.000001"
    assert normalize(payload(thread_ts="100.000001")).thread_ts is None
    assert normalize(payload(subtype="message_changed")) is None
    assert normalize(payload(user=None, bot_id="B1")) is None
    assert normalize(payload(ts="abc")) is None
    shared = normalize(payload(subtype="file_share", text="", files=[{"name": "plot.png"}]))
    assert shared.files == ("plot.png",)
    assert normalize(payload(bot_id="B1")).sender == "UALICE"  # other owners' Fridica posts keep their user


def test_metadata_round_trip_and_v1_compatibility():
    meta = FridicaMeta("UOWNER", session="T:C:1.0", turn=3, status="waiting", kind="report", worker="w1")
    parsed = normalize(payload(metadata=render.metadata(meta))).meta
    assert parsed == meta
    legacy = {"event_type": "fridica_message", "event_payload": {"owner": "UOLD", "task_id": "abc", "turn": 2, "status": "complete"}}
    old = normalize(payload(metadata=legacy)).meta
    assert (old.owner, old.session, old.turn, old.status, old.v) == ("UOLD", "abc", 2, "complete", 1)
    assert normalize(payload(metadata={"event_type": "other"})).meta is None
    assert render.parse_metadata({"event_type": "fridica_message", "event_payload": {"turn": -1, "status": "odd"}}).turn == 0


def test_dropped_mentions_are_described():
    assert "subtype=bot_message" in dropped_mention(payload(subtype="bot_message", text="<@UOWNER> hi"), "UOWNER")
    assert dropped_mention(payload(text="hi"), "UOWNER") is None


def test_render_helpers():
    text, details = render.fit_reply("word " * 50, "", 60)
    assert len(text) <= 60 and "details file" in text and details.startswith("word")
    assert render.fit_reply("", "doc", 60) == (render.ATTACHED, "doc")
    assert render.mentions("ask UBOB, not `UCAROL` or <@UDAN>", {"UBOB", "UCAROL"}) == "ask <@UBOB>, not `UCAROL` or <@UDAN>"
    assert render.reply_text("Which branch?", "", status="waiting", requester="UALICE", people=set(), limit=100)[0] == \
        "<@UALICE> Which branch?"
    links = render.permalinks("see https://t.slack.com/archives/C123/p1700000000000100?thread_ts=1699999999.000001 and again "
                              "https://t.slack.com/archives/C123/p1700000000000100")
    assert links == [("https://t.slack.com/archives/C123/p1700000000000100", "C123", "1700000000.000100", "1699999999.000001")]
    assert render.split_message("a\n\nb c d", 4) == ["a", "b c", "d"]


class Response(dict):
    def __init__(self, status, data=None, headers=None):
        super().__init__(data or {})
        self.status_code = status
        self.headers = headers or {}


class WebClient:
    def __init__(self, error=None):
        self.error = error
        self.calls = []

    async def chat_postMessage(self, **arguments):
        self.calls.append(arguments)
        if self.error:
            raise self.error
        return {"ts": "200.000001"}


@pytest.mark.parametrize("response, error", [
    (Response(429, {"error": "ratelimited"}, {"Retry-After": "7"}), RateLimited),
    (Response(500, {"error": "internal_error"}), DeliveryAmbiguous),
    (Response(200, {"error": "fatal_error"}), DeliveryAmbiguous),
    (Response(200, {"error": "channel_not_found"}), DeliveryRejected),
])
def test_egress_maps_errors(config, response, error):
    client = SlackClient(config, WebClient(SlackApiError("x", response)))
    with pytest.raises(error) as raised:
        asyncio.run(client.post("CROOM", "hi", thread_ts=None, meta=None))
    if error is RateLimited:
        assert raised.value.retry_after == 7.0


def test_egress_posts_with_metadata(config):
    web = WebClient()
    ts = asyncio.run(SlackClient(config, web).post("CROOM", "hi", thread_ts="1.0", meta=FridicaMeta("UOWNER", turn=1)))
    assert ts == "200.000001" and web.calls[0]["thread_ts"] == "1.0" and not web.calls[0]["unfurl_links"]
    assert web.calls[0]["metadata"]["event_payload"]["owner"] == "UOWNER"


def test_catch_up_stores_missed_messages_once(config, store):
    class Recent:
        async def recent(self, channel, oldest, threads=()):
            if channel != "CROOM":
                return []
            return [payload(ts="100.000009", text="<@UOWNER> missed")]

    bus = Bus()
    rung = []
    bus.on_thread(rung.append)
    assert asyncio.run(catchup.catch_up(Recent(), store, config, bus, 3600, 200.0)) == 1
    assert asyncio.run(catchup.catch_up(Recent(), store, config, bus, 3600, 200.0)) == 0
    assert rung == ["TTEAM:CROOM:100.000009"]
    assert store.messages.thread(store.threads.list()[0].key)[0].source == "catchup"
