#!/usr/bin/env python3
import json, os, pathlib, sys
arguments = sys.argv[1:]
prompt = sys.stdin.read()
pathlib.Path(os.environ["LLM_LOG"]).write_text(json.dumps({"argv": arguments, "prompt": prompt, "cwd": os.getcwd(),
                                                          "slack": os.environ.get("SLACK_USER_TOKEN")}))
mode = os.environ.get("MODE", "ok")
if mode == "exit":
    print("auth failed", file=sys.stderr); sys.exit(2)
envelope = {"type": "result", "is_error": mode == "error", "result": "oops", "session_id": "s",
            "structured_output": {"decision": "respond"}}
if mode == "tools":
    envelope["permission_denials"] = [{"tool_name": "Bash"}]
print(json.dumps(envelope))
