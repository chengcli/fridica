"""Capture pure frozen reply-policy cases; no daemon or external services.

Run with PYTHONPATH=spec. This is synthetic supported-input parity, not a
historical corpus or complete parent replay.
"""
import json
from pathlib import Path

from fridica.core.models import Message
from fridica.threads.policy import reply_hash, repost_requested

texts = [
    "can you repost that?", "post your sign-off again", "one more time please",
    "paste it again", "re-post", "REPOST", "again, verbatim", "again verbatim",
    "repeat your answer", "share the test result again", "send that again",
    "don't repeat that", "don’t repost", "don't ever repost", "do not repost",
    "never repost", "no need to repost", "we should repeat that run",
    "we should repeat the benchmark tomorrow", "thanks, noted", "repeat the test",
    "don't repost; paste it again", "<@UOWNER> don't repost", "<@USOMEONE> thanks",
    "prepost", "reposting", "DO NOT REPOST", "send this again", "say it again",
    "repeat that", "repeat this", "ONE MORE TIME", "",
    "don't send your repeat that again",
]
hashes = ["", "   ", "CI is green.", " CI IS\tGREEN.\n", "Straße", "STRASSE",
          "Σ σ ς", "İ", "a\x1cb\x1dc\x1ed\x1fe", "x\u00a0y\u2003z", "👩‍🔬 done"]
output = {
    "source": "frozen spec/fridica/threads/policy.py",
    "repost": [{"text": text, "expected": repost_requested(
        Message("e", "T", "C", "1.0", None, "UALICE", text), "UOWNER")}
        for text in texts],
    "hashes": [{"text": text, "expected": reply_hash(text)} for text in hashes],
}
Path("tests/corpus/reply_policy.json").write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
