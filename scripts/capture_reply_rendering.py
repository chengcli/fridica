"""Synthetic frozen reply rendering projections. Run with PYTHONPATH=spec."""

import json
from pathlib import Path

from fridica.slack.render import reply_text

texts = [
    "Done.",
    "",
    "  \n",
    "word " * 1800,
    "First paragraph.\n\n" + "Long line\n" * 100,
    "ask UBOB, not `UCAROL` or <@UDAN>; UUNKNOWN @UBOB xUBOB UBOB_suffix",
    "UBOB https://example.test/UBOB <https://site/UBOB|UBOB> ```\nUBOB\n``` UBOB",
    "αUBOB UBOBβ UBOB. WTEAM",
    "👩‍🔬 " * 4000,
    "\x1cUBOB\x1f",
    "\u0345UBOB UBOB\u0345 UBOB² ²UBOB",
    "https://example.test/UBOB\x1cUBOB",
]
rows = []
strings = []
indices = {}


def intern(text):
    if text not in indices:
        indices[text] = len(strings)
        strings.append(text)
    return indices[text]


for text in texts:
    for status in ["complete", "waiting"]:
        for limit in [50, 500, 7000]:
            case = dict(
                text=text,
                details="Extra details: UBOB",
                status=status,
                requester="UALICE",
                people=["UBOB", "UCAROL", "WTEAM"],
                limit=limit,
            )
            expected = [intern(value) for value in reply_text(**case)]
            case["text"] = intern(case["text"])
            case["details"] = intern(case["details"])
            rows.append(dict(input=case, expected=expected))

Path("tests/corpus/reply_rendering.json").write_text(
    json.dumps(dict(strings=strings, cases=rows), ensure_ascii=False, indent=2) + "\n"
)
