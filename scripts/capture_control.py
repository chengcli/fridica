"""Capture synthetic legacy control views, using only the frozen Python baseline.

No daemon, network, credentials or models are used. Rust checks the legacy fields
of these projections; v6 additions and authentication/effect changes are separate
Rust tests, not a claim of complete control API or historical replay parity.
"""
import json
from pathlib import Path
import tempfile
from fridica.store import Store, codec
from fridica.control import views

ROOT = Path(__file__).resolve().parents[1]
SESSION = "TTEAM:CROOM:100.1"
seed = []

def insert(table, **values):
    seed.append([f"INSERT INTO {table} ({','.join(values)}) VALUES ({','.join('?' for _ in values)})", list(values.values())])

insert("threads", id=SESSION, workspace="TTEAM", channel="CROOM", root_ts="100.1", created=10., updated=20., status="blocked", context_json='{"repo":"owner/repo"}', decisions_json='["Run checks"]')
insert("threads", id="old", workspace="TTEAM", channel="CROOM", root_ts="90.1", created=1., updated=2., control="paused", pause_reason="Owner pause")
insert("threads", id="recent", workspace="TTEAM", channel="CROOM", root_ts="110.1", created=21., updated=22.)
for n, source, meta in [(1,"socket",None),(2,"self",None),(3,"self",'{"owner":"UOWNER","kind":"reply","status":"complete","turn":1}')]:
    insert("messages",event_id=f"event-{n}",workspace="TTEAM",channel="CROOM",root_ts="100.1",ts=f"100.{n}",thread_ts=None if n==1 else "100.1",sender="UOWNER" if n>1 else "UASKER",text=f"Message {n}",source=source,meta_json=meta,received_at=20.)
for n in range(1,3):
    insert("workers",id=f"worker-{n}",session_id=SESSION,machine="local",workspace="project",backend="codex",created=float(n),updated=20.+n,status="running" if n==1 else "idle",last_result_json='{"status":"done","summary":"Finished","changes":[{"path":"a.py"}]}',ephemeral=n-1)
    insert("jobs",id=f"job-{n}",worker_id=f"worker-{n}",session_id=SESSION,brief="Run checks",queued_at=float(n),status="running" if n==1 else "queued",result_json='{"status":"done","summary":"Finished"}')
insert("jobs",id="job-3",worker_id="worker-1",session_id=SESSION,brief="Follow up",queued_at=3.,status="queued")
insert("jobs",id="job-4",worker_id="worker-2",session_id=SESSION,brief="Previously started",queued_at=4.,status="running",started_at=-1.)
insert("approvals",id="approval-2",worker_id="worker-2",job_id="job-4",session_id=SESSION,kind="exec",summary="Second command",created=21.,expires_at=300.)
insert("approvals",id="approval-1",worker_id="worker-1",job_id="job-1",session_id=SESSION,kind="exec",summary="Run command",detail_json='{"command":"echo ok"}',created=20.,expires_at=300.)
insert("outbox",idem_key="post-1",session_id=SESSION,kind="reply",channel="CROOM",thread_ts="100.1",text="Hello",created=20.,state="failed")
# SQL literal keeps binary content out of JSON while exercising blob omission.
seed.append(["INSERT INTO outbox(idem_key,session_id,kind,channel,thread_ts,text,blob,state,created) VALUES('post-2',?,'upload','CROOM','100.1','Report',X'010203','ambiguous',21)",[SESSION]])
insert("notes",session_id=SESSION,revision=1,actor="UOWNER",data_json='{"summary":"Progress"}',source="control",created=20.)
insert("thread_inbox",session_id=SESSION,kind="owner_instruction",ref="request-1234",payload_json='{"text":"Continue"}',created=20.)
insert("audit",time=20.,actor="owner",action="thread.pause",target=SESSION,details_json='{"reason":"Review"}')

with tempfile.TemporaryDirectory() as tmp:
    store=Store(Path(tmp)/"state.db")
    for sql, params in seed:
        store.db.execute(sql,params)
    def query(table, decode, view, order):
        return [view(decode(row)) for row in store.db.all(f"SELECT * FROM {table} ORDER BY {order}")]
    workers=query("workers",codec.worker,views.worker,"updated DESC,id")
    workers[1]["process"]="busy"
    jobs=[views.job(j) for w in store.workers.for_session(SESSION) for j in store.jobs.for_worker(w.id)]
    posts=query("outbox",codec.outbox,views.outbox,"id")
    revision, notes=store.notes.current(SESSION)
    cases=[
        {"target":"/threads?limit=1","expected":[views.session(store.threads.get("recent"))]},
        {"target":"/threads?control=paused","expected":[views.session(store.threads.get("old"))]},
        {"target":"/attention/threads?limit=1","expected":[views.session(s) for s in store.threads.needing_attention()]},
        {"target":"/workers","expected":workers},
        {"target":"/jobs","expected":[views.job(j) for j in store.jobs.running()+store.jobs.queued()]},
        {"target":"/approvals","expected":[views.approval(a) for a in store.approvals.list()]},
        {"target":"/outbox","expected":list(reversed(posts))},
        {"target":"/activity","expected":store.audit.recent()},
        {"target":f"/threads/{SESSION}","expected":{
            "session":views.session(store.threads.get(SESSION)),
            "messages":[views.message(m) for m in store.messages.thread(store.threads.get(SESSION).key)],
            "workers":list(reversed(workers)),"jobs":jobs,"outbox":posts,
            "notes":{"revision":revision,"data":notes},"instructions":store.inbox.instructions(SESSION),
        }},
    ]
    store.close()
(ROOT/"tests/corpus/control.json").write_text(json.dumps({"provenance":"Frozen Python v0.3.11 synthetic control views; compare legacy keys only", "exceptions":["v6 additive fields", "authority and durable acknowledgements tested separately", "bounded lists except attention selection; deterministic ID tie-breaks"],"seed":seed,"cases":cases},indent=2,sort_keys=True)+"\n")
print(f"Captured {len(cases)} frozen control projections")
