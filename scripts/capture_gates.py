"""Capture synthetic exact gate fixtures from the frozen Python reference.

Run with PYTHONPATH=spec. No historical corpus or external service is involved.
"""
from dataclasses import asdict
from itertools import product
import json
from pathlib import Path

from fridica.config.schema import Limits
from fridica.core.models import FridicaMeta, Message, ThreadSession
from fridica.threads.policy import gate


def main():
    rows = []
    modes = ("human", "mention", "owner", "self", "peer", "peer_mention", "debrief")
    for control, status, mode, turns, observe, resumed in product(
            ("active", "paused", "closed", "archived", "cleaned"),
            ("new", "waiting", "blocked", "working", "complete"), modes, (0, 2), (False, True), (False, True)):
        peer = mode in ("self", "peer", "peer_mention", "debrief")
        text = "<@UOWNER> help" if "mention" in mode else "hello"
        sender = "UOWNER" if mode in ("owner", "self") else "UALICE"
        meta = FridicaMeta("UPEER", turn=7, status="complete", kind="debrief_root" if mode == "debrief" else "reply") if peer else None
        message = Message("e1", "TTEAM", "CROOM", "100.1", None, sender, text, meta=meta)
        session = ThreadSession(message.key.id, message.key, status=status, control=control, turns=turns)
        kwargs = dict(owner="UOWNER", general_messages=True, cooling=False, observe_only=observe, resumed=resumed)
        expected = gate(message, session, limits=Limits(), **kwargs)
        inputs = {**kwargs, "text": text, "sender": sender, "generated": peer, "meta_kind": meta.kind if meta else "",
                  "meta_status": meta.status if meta else "", "peer_turn": meta.turn if meta else 0,
                  "control": control, "status": status, "turns": turns, "reset_at": 0., "ts": 100.1}
        rows.append({"id": str(len(rows)+1), "input": inputs, "expected": asdict(expected)})
    target = Path(__file__).resolve().parents[1] / "tests/corpus/gates.jsonl"
    target.parent.mkdir(exist_ok=True)
    target.write_text("".join(json.dumps(row, sort_keys=True, separators=(",", ":")) + "\n" for row in rows))


if __name__ == "__main__":
    main()
