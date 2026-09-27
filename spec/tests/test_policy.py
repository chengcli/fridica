from dataclasses import replace

import pytest

from fridica.config.schema import Limits
from fridica.core.models import FridicaMeta, Message, StickyContext, ThreadKey, ThreadSession
from fridica.threads.policy import advance, gate

SESSION = ThreadSession("T:C:1.0", ThreadKey("T", "C", "1.0"))
LIMITS = Limits(max_wait_replies=2, max_no_progress=2)


def message(text="hi", sender="UALICE", meta=None, ts="2.0"):
    return Message("e", "T", "C", ts, "1.0", sender, text, meta=meta)


def verdict(msg, session=SESSION, **options):
    values = {"owner": "UOWNER", "limits": LIMITS, "general_messages": True, "cooling": False}
    values.update(options)
    return gate(msg, session, **values)


@pytest.mark.parametrize("msg, session, options, expected", [
    (message("<@UOWNER> hi"), SESSION, {}, ("respond", 1)),
    (message("<@UOWNER> hi"), SESSION, {"observe_only": True}, ("observe", 0)),
    (message("<@UOWNER> hi"), replace(SESSION, control="paused"), {}, ("observe", 0)),
    (message("<@UOWNER> hi", ts="1.5"), replace(SESSION, reset_at=1.6), {}, ("observe", 0)),
    (message("<@UOWNER> hi", ts="1.5"), replace(SESSION, reset_at=1.6), {"resumed": True}, ("respond", 1)),
    (message("mine", sender="UOWNER"), SESSION, {}, ("observe", 0)),
    (message("mine", sender="UOWNER", meta=FridicaMeta("UOWNER")), SESSION, {}, ("ignore", 0)),
    (message("done", sender="UPEER", meta=FridicaMeta("UPEER", status="complete", turn=2)), SESSION, {}, ("ignore", 0)),
    (message("<@UOWNER> debrief", sender="UPEER", meta=FridicaMeta("UPEER", kind="debrief_root")), SESSION, {}, ("ignore", 0)),
    (message("<@UOWNER> ?", sender="UPEER", meta=FridicaMeta("UPEER", status="waiting", turn=2)), SESSION, {}, ("respond", 3)),
    (message("<@UOWNER> ?", sender="UPEER", meta=FridicaMeta("UPEER", turn=9)), SESSION, {}, ("respond", 10)),
    (message("<@UOWNER> ?"), replace(SESSION, turns=40), {}, ("respond", 41)),  # no turn limit
    (message("<@UOWNER> ?"), replace(SESSION, status="blocked"), {}, ("notice", 1)),
    (message("anyone?"), replace(SESSION, status="blocked"), {}, ("observe", 0)),
    (message("main branch"), replace(SESSION, status="waiting", turns=1), {}, ("respond", 2)),
    (message("also CUDA 13"), replace(SESSION, turns=1), {"general_messages": False}, ("triage", 2)),
    (message("anyone?"), SESSION, {}, ("triage", 1)),
    (message("anyone?"), SESSION, {"cooling": True}, ("observe", 0)),
    (message("anyone?"), SESSION, {"general_messages": False}, ("observe", 0)),
])
def test_gate(msg, session, options, expected):
    result = verdict(msg, session, **options)
    assert (result.kind, result.turn) == expected, result


def arguments(**changes):
    values = {"send": True, "status": "complete", "text": "hello", "turn": 1, "delegated": False, "working": False,
              "note_kind": "result", "summary": "", "decisions": (), "context": {}, "limits": LIMITS}
    values.update(changes)
    return values


def test_advance_counts_turns_and_merges_context():
    session = advance(SESSION, **arguments(summary="s", decisions=("d",), context={"machine": "snowy"}))
    assert (session.turns, session.status, session.summary, session.decisions) == (1, "complete", "s", ("d",))
    assert session.context == StickyContext(machine="snowy") and session.control == "active"
    working = advance(session, **arguments(delegated=True, turn=2, text="started"))
    assert working.status == "working" and working.no_progress == 0 and working.context.machine == "snowy"


def test_advance_pauses_loops_and_stalls():
    once = advance(SESSION, **arguments(status="waiting", text="which?"))
    twice = advance(once, **arguments(status="waiting", text="which one?", turn=2))
    assert twice.control == "paused" and "consecutive" in twice.pause_reason
    quiet = advance(SESSION, **arguments(send=False))
    stalled = advance(quiet, **arguments(note_kind="ack", text="thanks!", turn=2))
    assert stalled.control == "paused" and "without progress" in stalled.pause_reason
    repeat = advance(advance(SESSION, **arguments(text="Same")), **arguments(text="same ", turn=2))
    assert repeat.no_progress == 1


def test_peer_turns_count_after_a_resume():
    resumed = replace(SESSION, reset_at=1.5, turns=0)
    peer = message("<@UOWNER> ?", sender="UPEER", meta=FridicaMeta("UPEER", status="waiting", turn=7), ts="3.0")
    assert verdict(peer, resumed).turn == 8
