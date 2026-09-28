#!/usr/bin/env python3
import json, os, pathlib, sys
arguments = sys.argv[1:]
prompt = sys.stdin.read()
schema = arguments[arguments.index("--output-schema") + 1]
pathlib.Path(os.environ["LLM_LOG"]).write_text(json.dumps({"argv": arguments, "prompt": prompt,
                                                          "schema": json.load(open(schema))}))
mode = os.environ.get("MODE", "ok")
print(json.dumps({"type": "thread.started", "thread_id": "t"}))
if mode == "tools":
    print(json.dumps({"type": "item.completed", "item": {"type": "command_execution", "command": "ls"}}))
print(json.dumps({"type": "item.completed", "item": {"type": "agent_message", "text": json.dumps({"decision": "ignore"})}}))
