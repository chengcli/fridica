"""Synthetic frozen Python permalink, fetch selection and linked-context projections.

No network/model calls; not a historical corpus. Rust hardening exceptions are
explicit per fixture (ASCII/length checks and enforcing the reply-page limit).
"""
import asyncio
import hashlib
import json
from pathlib import Path
from fridica.core.models import Message
from fridica.slack import links, render
from fridica.slack.egress import SlackClient


def url(n=200, channel="CROOM"):
    return f"https://t.slack.com/archives/{channel}/p{n}000001"


def digest(value):
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()


def message(text, event="e"):
    return Message(event, "TTEAM", "CROOM", "100.000001", None, "UALICE", text)


async def main():
    parsers = []
    for name, text in [
        ("plain", url()), ("slack_markup", f"<{url()}?thread_ts=199.000001&cid=CROOM|thread>"),
        ("duplicate_target", url()+"?thread_ts=199.000001 "+url().replace("t.slack", "other.slack")),
        ("private_channel", url(channel="GPRIVATE")),
        ("three_links", " ".join(url(n) for n in range(200, 205))),
        ("not_slack", "https://t.slack.com.evil/archives/CROOM/p200000001"),
        ("not_https", url().replace("https", "http")),
        ("lowercase_channel", url(channel="Croom")),
        ("root_query", url()+"?x=1&thread_ts=199.000001&y=2"),
        ("root_suffix", url()+"?thread_ts=199.000001..."),
        ("missing_root", url()+"?thread_ts=no"),
        ("short_timestamp", "https://t.slack.com/archives/CROOM/p123456"),
        ("unicode_timestamp", url().replace("200000001", "２０００００００１")),
        ("long_timestamp", url().replace("200000001", "2"*40)),
    ]:
        expected = [dict(zip(("link", "channel", "ts", "root"), entry)) for entry in render.permalinks(text)]
        item = {"name": name, "text": text, "expected": expected}
        if name in ("unicode_timestamp", "long_timestamp"):
            item.update(rust_expected=[], exception="Rust accepts only bounded ASCII timestamps for fixed Slack API routes.")
        parsers.append(item)
    base = [{"ts":"200.000001","user":"UA","text":"root"},
            {"ts":"201.000001","thread_ts":"200.000001","bot_id":"B1","text":"reply"},
            {"ts":"202.000001","thread_ts":"200.000001","user":"UB","text":"next"}]
    fetches = []
    for name, messages, ts, root in [
        ("root", base, "200.000001", None),
        ("reply", base, "201.000001", "200.000001"),
        ("missing", base, "999.000001", None),
        ("bad_entries", [None, 3, {"text":4}, *base], "200.000001", None),
        ("explicit_root", [{**base[0],"thread_ts":"200.000001"}, *base[1:]], "200.000001", None),
        ("empty_user", [{"ts":"200.000001","user":"","bot_id":"B1","text":"hello"}], "200.000001", None),
        ("unicode_limit", [{"ts":"200.000001","text":"雪"*40001}], "200.000001", None),
        ("oversized_page", [{"ts":f"{n}.000001","text":"x"} for n in range(200, 260)], "200.000001", None),
    ]:
        calls = []
        class Web:
            async def conversations_replies(self, **kw):
                calls.append(kw)
                return {"messages":messages}
        expected = await SlackClient(None, Web()).fetch("CROOM", ts, root)
        item = {"name":name,"messages":messages,"ts":ts,"root":root,"sha256":digest(expected),"calls":calls}
        if name == "oversized_page":
            item.update(rust_sha256=digest(expected[:51]), exception="Rust enforces the requested 51-entry bound even if the server violates it.")
        fetches.append(item)
    contexts = []
    for name, text, history, data in [
        ("success", url(), [], {"200.000001":[{"sender":"UA","text":"linked"}]}),
        ("missing", url(), [], {}),
        ("failure", url(), [], {"200.000001":"error"}),
        ("scope", url(channel="COTHER"), [], {}),
        ("current_root", url(100), [], {}),
        ("current_reply", url()+"?thread_ts=100.000001", [], {}),
        ("ordering", url(204), [url(200),url(201),url(202),url(203)], {}),
        ("duplicate", url(), [url()], {}),
        ("unicode_budget", url()+" "+url(201), [], {"200.000001":[{"sender":"UA","text":"雪"*19999},{"sender":"UB","text":"😀abc"}]}),
        ("empty_then_text", url(), [], {"200.000001":[{"sender":"","text":""},{"sender":"UA","text":"x"}]}),
    ]:
        calls=[]
        class Slack:
            async def fetch(self, channel, ts, root):
                calls.append({"channel":channel,"ts":ts,"root":root})
                result=data.get(ts, [])
                if result == "error": raise RuntimeError("synthetic private detail")
                return result
        expected=await links.linked(Slack(), ("CROOM",), message(text), [message(t,str(i)) for i,t in enumerate(history)])
        contexts.append({"name":name,"messages":[{"text":t} for t in [text,*reversed(history)]],"data":data,"sha256":digest(expected),"calls":calls})
    path=Path(__file__).resolve().parents[1]/'tests/corpus/links.json'
    path.write_text(json.dumps({"parse":parsers,"fetch":fetches,"context":contexts},sort_keys=True,separators=(',',':'))+'\n')
    print(f'Captured {len(parsers)} permalink, {len(fetches)} fetch and {len(contexts)} linked-context cases.')

if __name__ == '__main__':
    asyncio.run(main())
