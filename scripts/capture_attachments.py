"""Capture frozen Python attachment rendering; synthetic bytes, no network/model.

Large outputs are hashed as canonical UTF-8 JSON to keep the corpus compact.
HTTP safety and durable snapshots are covered separately by Rust fault tests.
"""
import asyncio
from dataclasses import asdict
import hashlib
import json
from pathlib import Path
from fridica.core.models import Attachment, Message
from fridica.slack import files
from fridica.slack.egress import FileUnavailable

URL = "https://files.slack.com/files-pri/T-F/a"


def attachment(id="F1", name="a.diff", mimetype="text/x-diff", size=0, url=URL):
    return asdict(Attachment(id, name, mimetype, size, url))


def message(event, attachments):
    return {"event_id": event, "attachments": attachments}


def content(value):
    return value.get("text", "").encode() * value.get("repeat", 1) + bytes(value.get("suffix", []))


async def main():
    cases = []
    def add(name, messages, data, own=()):
        cases.append({"name":name,"messages":messages,"data":data,"own":list(own)})
    def one(label, data, **kw):
        add(label, [message("e1", [attachment(**kw)])], {URL:data})
    one("diff", {"text":"- old\n+ new\n"})
    one("large_unicode", {"text":"雪", "repeat":30000}, size=90000)
    one("unknown_size", {"text":"abc"})
    one("empty", {"text":""})
    one("nul", {"text":"abc\u0000def"})
    one("invalid_utf8", {"suffix":[255,254]})
    one("cut_invalid_utf8", {"text":"a", "repeat":65535,"suffix":[255,255,255,255]}, size=65539)
    one("html", {"text":"<p>report</p>"}, mimetype="text/html; charset=utf-8", name="r.html")
    one("scope_failure", {"error":"the Slack token lacks files:read"})
    for mime,name in [("image/png","a.txt"),("application/octet-stream","A.PATCH"),("","a.py"),("application/pdf","a.diff"),("application/json","a"),("","no-extension")]:
        one(f"type:{mime}:{name}", {"text":"ok"}, mimetype=mime, name=name)
    one("missing_url", {"text":"unused"}, url="")
    add("slots",[message("e1",[attachment(id=f"F{i}") for i in range(5)])],{URL:{"text":"ok"}})
    add("total",[message("e1",[attachment(id=f"F{i}") for i in range(2)])],{URL:{"text":"a","repeat":40960}})
    add("own_and_duplicates",[message("new",[attachment("FOWN"),attachment("FNEW")]),message("old",[attachment("FNEW"),attachment("FOLD")]),message("new",[attachment("FOTHER")])],{URL:{"text":"hi"}},("FOWN",))
    for case in cases:
        calls=[]
        class Slack:
            async def download(self,url,limit,*,html=False):
                calls.append({"url":url,"html":html})
                item=case["data"][url]
                if "error" in item:
                    raise FileUnavailable(item["error"])
                data=content(item)
                return data[:limit+1],item.get("size",len(data))
        messages=[Message(m["event_id"],"TTEAM","CROOM","100.1",None,"UALICE","hi",attachments=tuple(Attachment(**a) for a in m["attachments"])) for m in case["messages"]]
        expected=await files.read(Slack(),messages,own=set(case["own"]))
        canonical=json.dumps(expected,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()
        case["sha256"]=hashlib.sha256(canonical).hexdigest()
        case["calls"]=calls
    path=Path(__file__).resolve().parents[1]/'tests/corpus/attachments.json'
    path.write_text(json.dumps(cases,sort_keys=True,separators=(',',':'))+'\n')
    print(f'Captured {len(cases)} attachment rendering cases.')

if __name__ == '__main__':
    asyncio.run(main())
