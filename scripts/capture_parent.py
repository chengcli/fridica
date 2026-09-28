"""Capture frozen stateless parent command/envelope behavior; no model calls.

Strict Rust rejections of malformed/error-bearing Codex streams and incomplete
Claude envelopes are fixture-specific exceptions, not erased historical inputs.
"""
from copy import deepcopy
import ast
import itertools
import json
from pathlib import Path
from fridica.parent.llm import ClaudeLLM, CodexLLM
from fridica.parent.schemas import TRIAGE_SCHEMA
from fridica.threads.context import bounded, fit_attachments


def main():
    root=Path(__file__).resolve().parents[1]
    commands=[]
    for backend,model,effort in itertools.product(("codex","claude"),("","model with spaces"),("","high")):
        adapter=(CodexLLM if backend=="codex" else ClaudeLLM)(reasoning_effort=effort)
        commands.append({"backend":backend,"model":model,"effort":effort,"expected":adapter.command(Path("/private-parent"),json.dumps(TRIAGE_SCHEMA),model)})
    good={"type":"item.completed","item":{"type":"agent_message","text":'{"decision":"respond"}'}}
    inputs=[("codex",json.dumps(good),None),
            ("codex",json.dumps({"type":"thread.started","thread_id":"t"})+'\n'+json.dumps(good),None),
            ("codex",'garbage\n'+json.dumps(good),"Malformed JSONL cannot hide tool execution"),
            ("codex",'[]\n'+json.dumps(good),"Every stream event must be an object"),
            ("codex",json.dumps({"type":"turn.failed","error":{"message":"private"}})+'\n'+json.dumps(good),"An error event must not be masked by a later message"),
            ("codex",json.dumps({"type":"unknown"})+'\n'+json.dumps(good),"Unknown event envelopes fail closed"),
            ("codex",json.dumps({"type":"item.completed","item":{"type":"command_execution"}})+'\n'+json.dumps(good),None),
            ("codex",json.dumps({"type":"item.completed","item":{"type":"agent_message","text":"[]"}}),None),
            ("codex","",None),
            ("claude",json.dumps({"is_error":False,"structured_output":{"decision":"respond"}}),None),
            ("claude",json.dumps({"structured_output":{"decision":"respond"}}),"A successful Claude envelope must explicitly say is_error=false"),
            ("claude",json.dumps({"is_error":False,"permission_denials":[],"structured_output":{"decision":"respond"}}),None),
            ("claude",json.dumps({"is_error":False,"permission_denials":[{"tool_name":"Bash"}],"structured_output":{"decision":"respond"}}),None),
            ("claude",json.dumps({"is_error":True,"result":"private error"}),None),
            ("claude","[]",None),("claude","not json",None)]
    envelopes=[]
    for backend,output,exception in inputs:
        adapter=CodexLLM() if backend=="codex" else ClaudeLLM()
        try: expected=adapter.parse(output)
        except Exception: expected=None
        envelopes.append({"backend":backend,"output":output,"expected":expected,"rust_rejection":exception})
    budgets=[]
    for budget in [0,1,40,60,61,62,63,64,65,66,67,68,69,70,80,100,1000,10000]:
        views=[{"text":"雪"*4000},{"text":"b"*100},{"kind":"metadata"}]
        expected=deepcopy(views)
        remaining=fit_attachments(expected,budget)
        history=[{"text":"a"*100},{"text":"b"*200},{"text":"c"*40}]
        budgets.append({"budget":budget,"views":views,"expected":expected,"remaining":remaining,"history":history,"bounded":bounded(history,budget)})
    tree=ast.parse((root/'spec/tests/test_parent_llm.py').read_text())
    for node in tree.body:
        if isinstance(node,ast.Assign) and isinstance(node.targets[0],ast.Name) and node.targets[0].id in ('FAKE_CLAUDE','FAKE_CODEX'):
            backend=node.targets[0].id.removeprefix('FAKE_').lower()
            (root/f'tests/corpus/fake_parent_{backend}.py').write_text(ast.literal_eval(node.value).replace('{python}','/usr/bin/env python3'))
    (root/'tests/corpus/parent.json').write_text(json.dumps({"schema":TRIAGE_SCHEMA,"commands":commands,"envelopes":envelopes,"budgets":budgets},sort_keys=True,separators=(',',':'))+'\n')
    print(f'Captured {len(commands)} parent commands and {len(envelopes)} envelopes.')

if __name__=='__main__': main()
