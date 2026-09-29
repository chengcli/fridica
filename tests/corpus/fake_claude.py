#!/usr/bin/env python3
"""A minimal claude -p stream-json process with the SDK control protocol."""
import json, os, pathlib, sys
arguments = sys.argv[1:]
log = pathlib.Path(os.environ["WORKER_LOG"])
assert arguments[:6] == ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"], arguments
session = arguments[arguments.index("--resume") + 1] if "--resume" in arguments else arguments[arguments.index("--session-id") + 1]
if "--resume" in arguments and session == os.environ.get("LOST_SESSION"):
    print(f"No conversation found with session ID: {session}", file=sys.stderr)
    sys.exit(1)
def emit(message):
    sys.stdout.write(json.dumps(message) + "\n"); sys.stdout.flush()
def record(kind, data):
    with log.open("a") as stream:
        stream.write(json.dumps({"backend": "claude", "kind": kind, "data": data, "argv": arguments, "cwd": os.getcwd(), "pid": os.getpid()}) + "\n")
def final(summary, prose="Done."):
    body = json.dumps({"status": "done", "summary": summary, "changes": [], "validation": [], "artifacts": [],
                       "machine_state": {"branch": "dev", "commit": "", "dirty": False, "notes": ""}, "unresolved": [],
                       "question": "", "report": "All green on claude."})
    emit({"type": "result", "subtype": "success", "is_error": False, "result": f"{prose}\n```json\n{body}\n```", "session_id": session})
emit({"type": "system", "subtype": "init", "session_id": session,
      "permissionMode": os.environ.get("FAKE_PERMISSION_MODE") or arguments[arguments.index("--permission-mode") + 1]})
lines = iter(sys.stdin)
for line in lines:
    message = json.loads(line)
    record(message.get("type"), message)
    if message["type"] == "control_request":
        if message["request"]["subtype"] == "initialize":
            emit({"type": "control_response", "response": {"subtype": "success", "request_id": message["request_id"], "response": {}}})
        continue
    text = message["message"]["content"][0]["text"]
    if "TOOL:" in text:
        command = text.split("TOOL:", 1)[1].split()[0]
        emit({"type": "control_request", "request_id": "perm-1", "request": {"subtype": "can_use_tool", "tool_name": "Bash",
              "input": {"command": command}, "permission_suggestions": []}})
        answer = json.loads(next(lines))
        record("approval-answer", answer)
        final("tool " + answer["response"]["response"]["behavior"])
    elif "HANG" in text:
        for waiting in lines:
            request = json.loads(waiting)
            record(request.get("type"), request)
            if request.get("type") == "control_request" and request["request"]["subtype"] == "interrupt":
                emit({"type": "result", "subtype": "error_during_execution", "is_error": True, "result": "interrupted", "session_id": session})
                break
    elif "FAIL" in text:
        emit({"type": "result", "subtype": "error_during_execution", "is_error": True, "result": "PRIVATE FAILURE", "session_id": session})
    else:
        emit({"type": "assistant", "message": {"content": [{"type": "text", "text": "working"}]}})
        final(f"cwd={os.getcwd()} session={session}")
