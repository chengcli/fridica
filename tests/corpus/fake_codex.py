#!/usr/bin/env python3
"""A minimal codex app-server: JSON-RPC over JSONL on stdio, driven by the prompt text."""
import json, os, pathlib, sys
assert sys.argv[1] == "app-server", sys.argv
log = pathlib.Path(os.environ["WORKER_LOG"])
def emit(message):
    sys.stdout.write(json.dumps(message) + "\n"); sys.stdout.flush()
def record(kind, data):
    with log.open("a") as stream:
        stream.write(json.dumps({"backend": "codex", "kind": kind, "data": data, "argv": sys.argv, "cwd": os.getcwd(),
                                 "omp": os.environ.get("OMP_NUM_THREADS"), "pid": os.getpid(),
                                 "slack": os.environ.get("SLACK_USER_TOKEN")}) + "\n")
def result(summary, report="Done.", status="done", artifacts=()):
    return json.dumps({"status": status, "summary": summary, "changes": [{"path": "src/a.cpp", "change": "modified", "note": ""}],
                       "validation": [{"command": "pytest", "outcome": "passed", "detail": "12 passed"}],
                       "artifacts": list(artifacts), "machine_state": {"branch": "main", "commit": "abc", "dirty": True, "notes": ""},
                       "unresolved": [], "question": "", "report": report})
initialized, thread_id, counter = False, None, 0
lines = iter(sys.stdin)
for line in lines:
    message = json.loads(line)
    method, identifier, params = message.get("method"), message.get("id"), message.get("params") or {}
    record(method or "response", message)
    if method == "initialize":
        initialized = True
        emit({"id": identifier, "result": {"userAgent": "fake"}})
    elif method == "initialized":
        pass
    elif not initialized:
        emit({"id": identifier, "error": {"code": -32002, "message": "Not initialized"}})
    elif method == "config/read":
        servers = {"unexpected": {"command": "sh"}} if os.environ.get("FAKE_MCP_CONFIG") else {}
        emit({"id": identifier, "result": {"config": {"mcp_servers": servers}}})
    elif method == "thread/resume":
        if params["threadId"] == os.environ.get("LOST_THREAD"):
            emit({"id": identifier, "error": {"code": 1, "message": "no rollout found for thread id"}})
        else:
            thread_id = params["threadId"]
            emit({"id": identifier, "result": {"thread": {"id": thread_id}}})
    elif method == "thread/start":
        thread_id = "thr_" + str(os.getpid())
        emit({"id": identifier, "result": {"thread": {"id": thread_id}}})
    elif method == "turn/start":
        assert params["threadId"] == thread_id, (params, thread_id)
        counter += 1
        turn = f"turn_{counter}"
        emit({"id": identifier, "result": {"turn": {"id": turn, "status": "inProgress", "items": []}}})
        prompt = params["input"][0]["text"]
        def done(text):
            emit({"method": "item/completed", "params": {"threadId": thread_id, "turnId": turn, "item": {"id": "i", "type": "agentMessage", "text": text}}})
            emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": turn, "status": "completed", "items": []}}})
        if "APPROVE:" in prompt:
            command = prompt.split("APPROVE:", 1)[1].split()[0]
            emit({"id": 900 + counter, "method": "item/commandExecution/requestApproval",
                  "params": {"threadId": thread_id, "turnId": turn, "itemId": "c", "command": command, "startedAtMs": 1}})
            interrupt = None
            for waiting in lines:
                answer = json.loads(waiting)
                if answer.get("method") == "turn/interrupt":
                    interrupt = answer
                    record("turn/interrupt", answer)
                    continue
                if answer.get("id") == 900 + counter:
                    break
            record("approval-answer", answer)
            if interrupt is not None:
                emit({"id": interrupt["id"], "result": {}})
                emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": turn, "status": "interrupted", "items": []}}})
            else:
                done(result("approval " + str(answer["result"]["decision"])))
        elif "PERMS" in prompt:
            emit({"id": 950, "method": "item/permissions/requestApproval", "params": {"threadId": thread_id, "turnId": turn,
                  "itemId": "p", "cwd": "/", "startedAtMs": 1, "permissions": {"network": {"enabled": True}}}})
            answer = json.loads(next(lines))
            record("approval-answer", answer)
            done(result("perms " + json.dumps(answer["result"])))
        elif "ELICIT" in prompt:
            emit({"id": 960, "method": "mcpServer/elicitation/request", "params": {}})
            answer = json.loads(next(lines))
            record("elicit-answer", answer)
            done(result("elicited"))
        elif "HANG" in prompt:
            for waiting in lines:
                request = json.loads(waiting)
                record(request.get("method") or "response", request)
                if request.get("method") == "turn/interrupt":
                    emit({"id": request["id"], "result": {}})
                    emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": turn, "status": "interrupted", "items": []}}})
                    break
        elif "FAIL" in prompt:
            emit({"method": "turn/completed", "params": {"threadId": thread_id, "turn": {"id": turn, "status": "failed", "items": [], "error": {"message": "model refused"}}}})
        elif "EXIT" in prompt:
            print("fatal: boom", file=sys.stderr)
            sys.exit(3)
        elif "PROSE" in prompt:
            done("I looked around and fixed the thing, but here is no JSON.")
        elif "Return only the WorkerResult" in prompt:
            done(result("summarized after prose", report="Fixed the thing."))
        elif "ARTIFACT:" in prompt:
            path = prompt.split("ARTIFACT:", 1)[1].split()[0]
            done(result("made a plot", artifacts=[{"path": path, "kind": "png", "caption": "the plot"}]))
        else:
            done(result(f"cwd={os.getcwd()} prompt={prompt[:60]}", report="All green."))
    else:
        emit({"id": identifier, "error": {"code": -32601, "message": "unknown method"}})
