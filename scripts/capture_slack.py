"""Frozen Slack normalization and catch-up fixtures; no network calls.

New attention obligations and complete boundary recordings are tested separately.
Only the listed state projection is a Python/Rust parity assertion.
"""
import asyncio
from copy import deepcopy
from dataclasses import asdict
import json
from pathlib import Path
import tempfile
from fridica.config import load_config
from fridica.core.bus import Bus
from fridica.core.models import Message, FridicaMeta
from fridica.core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from slack_sdk.errors import SlackApiError
from fridica.slack import catchup
from fridica.slack.egress import SlackClient, IncompleteHistory
from fridica.slack.ingress import normalize
from fridica.store import Store

NOW = 2_000_000_000.0
DAY = 86400.0
ROOT = Path(__file__).resolve().parents[1]


def payload(**overrides):
    return {"type":"event_callback","event_id":"Ev1","team_id":"TTEAM","event":{
        "type":"message","channel":"CROOM","user":"UALICE","text":"hello","ts":"100.000001",**overrides}}


def message(ts, text="hello", **kw):
    return {"ts":f"{ts:.6f}","text":text,"user":"UALICE",**kw}


def projection(store):
    return {"messages":[dict(r) for r in store.db.all("SELECT event_id,ts,thread_ts,text,files_json,source,meta_json,attachments_json FROM messages ORDER BY id")],
            "threads":[dict(r) for r in store.db.all("SELECT id,status,created,updated FROM threads ORDER BY id")],
            "inbox":[dict(r) for r in store.db.all("SELECT session_id,kind,ref,state FROM thread_inbox ORDER BY id")],
            "mark":store.db.meta("catchup:TTEAM:CROOM"),"truncated":store.db.meta("catchup:TTEAM:CROOM:truncated")}


async def capture(case):
    with tempfile.TemporaryDirectory() as tmp:
        root=Path(tmp)
        (root/'project').mkdir()
        path=root/'config.toml'
        path.write_text(f'''[owner]
slack_user="UOWNER"
[slack]
workspace="TTEAM"
channels=["CROOM"]
[machines.local]
backends=["codex"]
[machines.local.workspaces]
project="{root/'project'}"
[state]
path="{root/'db'}"
''')
        config=load_config(path)
        store=Store(root/'db')
        for index,seed in enumerate(case.get('seeds',[])):
            msg=Message(f"seed{index}","TTEAM","CROOM",f"{seed['ts']:.6f}",seed.get('thread_ts'),"UALICE",seed.get('text','earlier'))
            store.messages.intake(msg,seed.get('received',seed['ts']))
            if seed.get('waiting'):
                store.db.execute("UPDATE threads SET status='waiting' WHERE id=?",(msg.key.id,))
        for run in case['runs']:
            calls=[]
            responses=[]
            class Web:
                async def conversations_history(self,**arguments):
                    more=run.get('truncated',False)
                    response={"ok":True,"messages":deepcopy(run.get('messages',[])),"response_metadata":{"next_cursor":"more" if more else ""}}
                    calls.append({"method":"conversations.history",**arguments})
                    responses.append(response)
                    return response
                async def conversations_replies(self,**arguments):
                    response={"ok":True,"messages":deepcopy(run.get('replies',{}).get(arguments['ts'],[])),"response_metadata":{"next_cursor":""}}
                    calls.append({"method":"conversations.replies",**arguments})
                    responses.append(response)
                    return response
            try:
                added=await catchup.catch_up(SlackClient(config,Web()),store,config,Bus(),run.get('window',3600.),run['now'],run.get('started'))
                incomplete=False
            except IncompleteHistory:
                added=None
                incomplete=True
            run.update(calls=calls,responses=responses,expected=projection(store),incomplete=incomplete,added=added)
        store.close()
    return case


async def capture_web():
    class Response(dict):
        def __init__(self,status,body,headers):
            super().__init__(body)
            self.status_code=status
            self.headers=headers
    cases=[]
    meta=FridicaMeta("UOWNER",session="TTEAM:CROOM:100.1",turn=3,status="waiting",kind="report",worker="w1")
    inputs=[(200,{"ok":True,"ts":"200.1"},{}),
            (200,{"ok":True},{}),
            (200,{"ok":True,"ts":"invalid"},{}),
            (200,{"ok":False,"error":"channel_not_found"},{}),
            (500,{"ok":False,"error":"internal_error"},{}),
            (200,{"ok":False,"error":"fatal_error"},{}),
            (200,{"ok":False,"error":"request_timeout"},{}),
            (200,{"ok":False,"error":"service_unavailable"},{}),
            (429,{"ok":False,"error":"ratelimited"},{"Retry-After":"7"}),
            (429,{"ok":False},{"Retry-After":"NaN"}),
            (429,{"ok":False},{"Retry-After":"invalid"})]
    for status,body,headers in inputs:
        calls=[]
        class Web:
            async def chat_postMessage(self,**arguments):
                calls.append(arguments)
                if status!=200 or body.get('ok') is not True:
                    raise SlackApiError('fixture',Response(status,body,headers))
                return body
        try:
            reference=await SlackClient(None,Web()).post("CROOM","hello",thread_ts="100.1",meta=meta)
            expected={"outcome":"sent","reference":reference}
        except RateLimited as error:
            expected={"outcome":"rate_limited","retry_after":error.retry_after}
        except DeliveryAmbiguous:
            expected={"outcome":"ambiguous"}
        except DeliveryRejected:
            expected={"outcome":"rejected"}
        cases.append({"status":status,"body":body,"headers":headers,"expected":expected,"request":calls[0]})
    return cases


async def main():
    normalization=[]
    inputs=[payload(),payload(thread_ts='100.000001'),payload(ts='100.000002',thread_ts='100.000001'),
            payload(bot_id='B1'),payload(user=None,bot_id='B1'),payload(subtype='bot_message'),payload(subtype='message_changed'),
            payload(subtype='thread_broadcast'),payload(subtype=123),payload(text=None),payload(text='',files=[{'name':'plot.png'}]),
            payload(text='',files=[None]),payload(text='',files='bad'),payload(text='雪'*40001),
            payload(ts='abc'),payload(ts='1'),payload(ts='1.'),payload(ts='.1'),payload(ts='NaN'),payload(ts='1.1.1'),
            payload(thread_ts=1),payload(thread_ts='abc'),None,[],{}, {'type':'other'}]
    meta={'event_type':'fridica_message','event_payload':{'owner':'UPEER','session':'T:C:1.0','turn':3,'status':'waiting','kind':'report','worker':'w1','v':2}}
    for data in [meta,{'event_type':'other'}, {'event_type':'fridica_message'},
                 {'event_type':'fridica_message','event_payload':{'task_id':'legacy','turn':-1,'status':'odd'}},
                 {'event_type':'fridica_message','event_payload':{'turn':True,'owner':42,'v':9}},
                 {'event_type':'fridica_message','event_payload':{'turn':10001,'owner':'雪'*40,'kind':'x'*40}}]:
        inputs.append(payload(metadata=data))
    urls=['https://files.slack.com/files/x','HTTPS://FILES.SLACK.COM/x','http://files.slack.com/x','https://files.slack.com:443/x',
          'https://files.slack.com@evil.test/x','https://u@files.slack.com/x','https://files.slack.com.evil/x',
          'https://files.slack.com\\@evil/x','https://files.slack.com/#x','https://files.slack.com?x',
          'https://files.slack.com/x\ny',' https://files.slack.com/x','https://files.slack.com/a b']
    for url in urls:
        inputs.append(payload(files=[{'id':'F1','name':'a.txt','mimetype':'text/plain','size':7,'url_private':url}]))
    for item in inputs:
        result=normalize(item)
        value=asdict(result) if result else None
        exception=None
        if result and any(a.url and any(c.isspace() for c in a.url) for a in result.attachments):
            exception='Reject file URLs containing whitespace or control characters before any credentialed download.'
            value['attachments'][0]['url']=''
        normalization.append({'payload':item,'python':asdict(result) if result else None,'expected':value,'exception':exception})
    for item in [payload(ts='١.٢'),payload(thread_ts='9'*400+'.1')]:
        normalization.append({'payload':item,'python':asdict(normalize(item)),'expected':None,'exception':'Reject non-ASCII or nonfinite Slack timestamps, including thread roots.'})
    cases=[
        {'name':'fresh','runs':[{'now':NOW}]},
        {'name':'fresh_truncation','runs':[{'now':NOW+i*300,'window':3600. if i==0 else 900.,'truncated':i<2,'messages':[message(NOW-60)] if i<2 else []} for i in range(3)]},
        {'name':'outage','seeds':[{'ts':NOW-18000}], 'runs':[{'now':NOW,'messages':[message(NOW-18000+600*i) for i in range(1,31)]}]},
        {'name':'seven_days','seeds':[{'ts':NOW-30*DAY}], 'runs':[{'now':NOW}]},
        {'name':'startup_live','seeds':[{'ts':NOW-3*DAY},{'ts':NOW-10}], 'runs':[{'now':NOW,'started':NOW-60,'messages':[message(NOW-2*DAY,'<@UOWNER> outage')]}]},
        {'name':'old_waiting','seeds':[{'ts':NOW-3*DAY,'waiting':True}], 'runs':[{'now':NOW,'replies':{f'{NOW-3*DAY:.6f}':[message(NOW-2*DAY,'yes',thread_ts=f'{NOW-3*DAY:.6f}')]}}]},
        {'name':'overlap','seeds':[{'ts':NOW-3*DAY}], 'runs':[{'now':NOW,'messages':[message(NOW-3*DAY),message(NOW-3*DAY+3600),message(NOW-3*DAY+7200,'<@UOWNER> ask'),message(NOW-3600)]}]},
        {'name':'truncation','seeds':[{'ts':NOW-2*DAY}], 'runs':[{'now':NOW+i*300,'window':3600. if i==0 else 900.,'truncated':i<3,'messages':[message(NOW-60)] if i<3 else []} for i in range(4)]},
        {'name':'recent','seeds':[{'ts':NOW-30}], 'runs':[{'now':NOW,'window':900.}]},
        {'name':'root_replies','runs':[{'now':NOW,'messages':[message(NOW-300,reply_count=2)],'replies':{f'{NOW-300:.6f}':[message(NOW-300),message(NOW-200,thread_ts=f'{NOW-300:.6f}'),message(NOW-100,thread_ts=f'{NOW-300:.6f}')]}}]},
    ]
    overlap=next(case for case in cases if case['name']=='overlap')
    overlap['runs'].append(deepcopy(overlap['runs'][0]))
    captured=[await capture(case) for case in cases]
    bad_page={"ok":True,"messages":[],"has_more":True}
    async def incomplete_page(**arguments): return bad_page
    _,python_complete=await SlackClient._pages(incomplete_page)
    paging_exception={"response":bad_page,"python_complete":python_complete,"rust_complete":False,
                      "reason":"has_more without a cursor cannot certify a complete pass"}
    (ROOT/'tests/corpus/slack.json').write_text(json.dumps({'normalization':normalization,'catchup':captured,'paging_exception':paging_exception,'web':await capture_web()},sort_keys=True,separators=(',',':'))+'\n')
    print(f'Captured {len(normalization)} normalization cases and {len(captured)} catch-up scenarios.')

if __name__=='__main__': asyncio.run(main())
