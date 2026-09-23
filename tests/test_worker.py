"""Persistent heavy-task workers against fake ``codex app-server`` and ``claude`` stream-json processes."""
import asyncio
from dataclasses import replace
import json
import os
from pathlib import PurePosixPath
import sys

import pytest

from fridica.config import Resources
from fridica.runner import BackendError
from fridica.worker import ClaudeWorker, CodexWorker, Workers

FAKE_APP_SERVER = f'''#!{sys.executable}
"""A minimal codex app-server: JSON-RPC over JSONL on stdio, driven by environment variables."""
import json, os, pathlib, sys, time
assert sys.argv[1:3] == ["app-server", "-c"], sys.argv
log = pathlib.Path(os.environ["WORKER_LOG"])
def emit(message):
    sys.stdout.write(json.dumps(message) + "\\n"); sys.stdout.flush()
def record(kind, data):
    with log.open("a") as stream:
        stream.write(json.dumps({{"kind": kind, "data": data, "cwd": os.getcwd(), "omp": os.environ.get("OMP_NUM_THREADS"), "cuda": os.environ.get("CUDA_VISIBLE_DEVICES"), "pid": os.getpid()}}) + "\\n")
initialized = False
thread_id = None
for line in sys.stdin:
    message = json.loads(line)
    method, identifier, params = message.get("method"), message.get("id"), message.get("params") or {{}}
    record(method or "response", message)
    if method == "initialize":
        initialized = True
        emit({{"id": identifier, "result": {{"userAgent": "fake"}}}})
    elif method == "initialized":
        pass
    elif not initialized:
        emit({{"id": identifier, "error": {{"code": -32002, "message": "Not initialized"}}}})
    elif method == "thread/resume":
        if params["threadId"] == os.environ.get("LOST_THREAD"):
            emit({{"id": identifier, "error": {{"code": 1, "message": "no rollout found for thread id"}}}})
        else:
            thread_id = params["threadId"]
            emit({{"id": identifier, "result": {{"thread": {{"id": thread_id}}, "cwd": params.get("cwd"), "model": "m"}}}})
    elif method == "thread/start":
        assert params["approvalPolicy"] == "never" and params["sandbox"] == "workspace-write", params
        thread_id = "thr_" + os.environ.get("THREAD_SUFFIX", "1")
        emit({{"id": identifier, "result": {{"thread": {{"id": thread_id}}, "cwd": params["cwd"], "model": "m"}}}})
    elif method == "turn/start":
        assert params["threadId"] == thread_id, (params, thread_id)
        turn = "turn_" + str(int(time.time() * 1000))
        emit({{"id": identifier, "result": {{"turn": {{"id": turn, "status": "inProgress", "items": []}}}}}})
        emit({{"method": "turn/started", "params": {{"threadId": thread_id, "turn": {{"id": turn, "status": "inProgress", "items": []}}}}}})
        prompt = params["input"][0]["text"]
        if os.environ.get("ASK_APPROVAL"):
            emit({{"id": 900, "method": "item/commandExecution/requestApproval", "params": {{"threadId": thread_id, "turnId": turn, "itemId": "item_1", "command": "rm -rf build"}}}})
            answer = json.loads(sys.stdin.readline())
            record("approval-answer", answer)
            assert answer == {{"id": 900, "result": {{"decision": "decline"}}}}, answer
        emit({{"method": "item/agentMessage/delta", "params": {{"threadId": thread_id, "turnId": turn, "itemId": "item_2", "delta": "partial"}}}})
        if "FAIL" in prompt:
            emit({{"method": "turn/completed", "params": {{"threadId": thread_id, "turn": {{"id": turn, "status": "failed", "items": [], "error": {{"message": "model refused: PRIVATE DETAIL"}}}}}}}})
            continue
        if "HANG" in prompt:
            time.sleep(30)
        if "EXIT" in prompt:
            print("fatal: PRIVATE STDERR", file=sys.stderr)
            sys.exit(3)
        report = os.environ.get("REPORT", "Ran the suite: 120 passed.") + (" [resumed]" if params.get("resumed") else "")
        emit({{"method": "item/completed", "params": {{"threadId": thread_id, "turnId": turn, "completedAtMs": 1, "item": {{"id": "item_2", "type": "agentMessage", "text": report}}}}}})
        emit({{"method": "turn/completed", "params": {{"threadId": thread_id, "turn": {{"id": turn, "status": "completed", "items": [], "error": None}}}}}})
    else:
        emit({{"id": identifier, "error": {{"code": -32601, "message": "unknown method"}}}})
'''

FAKE_CLAUDE_STREAM = f'''#!{sys.executable}
import json, os, pathlib, sys
arguments = sys.argv[1:]
log = pathlib.Path(os.environ["WORKER_LOG"])
assert arguments[:6] == ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"], arguments
assert arguments[arguments.index("--permission-mode") + 1] == "acceptEdits"
assert "--json-schema" not in arguments and "--output-format" in arguments
session = arguments[arguments.index("--resume") + 1] if "--resume" in arguments else arguments[arguments.index("--session-id") + 1]
with log.open("a") as stream:
    stream.write(json.dumps({{"argv": arguments, "cwd": os.getcwd(), "omp": os.environ.get("OMP_NUM_THREADS")}}) + "\\n")
sys.stdout.write(json.dumps({{"type": "system", "subtype": "init", "session_id": session}}) + "\\n"); sys.stdout.flush()
for line in sys.stdin:
    message = json.loads(line)
    assert message["type"] == "user" and message["message"]["content"][0]["type"] == "text"
    text = message["message"]["content"][0]["text"]
    if "FAIL" in text:
        sys.stdout.write(json.dumps({{"type": "result", "subtype": "error_during_execution", "is_error": True, "result": "PRIVATE FAILURE", "session_id": session}}) + "\\n")
    else:
        sys.stdout.write(json.dumps({{"type": "assistant", "message": {{"content": [{{"type": "text", "text": "thinking"}}]}}}}) + "\\n")
        sys.stdout.write(json.dumps({{"type": "result", "subtype": "success", "is_error": False, "result": "Job finished: all green.", "session_id": session}}) + "\\n")
    sys.stdout.flush()
'''


@pytest.fixture
def heavy_config(config):
    return replace(config, heavy_tasks=True, heavy_task_timeout=5, heavy_task_idle=0.3,
                   resources=Resources(cpus=4, gpus=(0, 1), gpu_type="A100"))


@pytest.fixture
def fake_worker_cli(tmp_path, monkeypatch):
    log = tmp_path / "worker.log"
    monkeypatch.setenv("WORKER_LOG", str(log))
    monkeypatch.setenv(os.environ.get("FRIDICA_UNUSED", "SLACK_APP_TOKEN"), "xapp-secret")
    binaries = tmp_path / "bin"
    binaries.mkdir()
    for name, body in (("codex", FAKE_APP_SERVER), ("claude", FAKE_CLAUDE_STREAM)):
        executable = binaries / name
        executable.write_text(body)
        executable.chmod(0o700)
    monkeypatch.setenv("PATH", str(binaries) + os.pathsep + os.environ["PATH"])

    def entries():
        return [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
    return entries


def test_codex_worker_job_resume_and_idle(heavy_config, fake_worker_cli, monkeypatch):
    config = replace(heavy_config, backend="codex", model="gpt-x", allowed_domains=("github.com",))

    async def scenario():
        worker = CodexWorker(config)
        report, thread = await worker.run("Run the suite", None)
        assert report == "Ran the suite: 120 passed." and thread == "thr_1"
        first_pid = fake_worker_cli()[0]["pid"]
        start = [entry for entry in fake_worker_cli() if entry["kind"] == "thread/start"][0]
        assert start["data"]["params"]["cwd"] == str(config.workspace) and start["data"]["params"]["model"] == "gpt-x"
        assert start["cwd"] == str(config.workspace) and start["omp"] == "4" and start["cuda"] == "0,1"
        turn = [entry for entry in fake_worker_cli() if entry["kind"] == "turn/start"][0]["data"]["params"]
        assert turn["sandboxPolicy"] == {"type": "workspaceWrite", "networkAccess": True, "writableRoots": []}
        assert turn["input"] == [{"type": "text", "text": "Run the suite"}]
        # A second job on the live process needs no new handshake.
        report, thread = await worker.run("Again", thread)
        assert thread == "thr_1" and worker.alive
        assert sum(entry["kind"] == "initialize" for entry in fake_worker_cli()) == 1
        # Idle shutdown, then resume by thread id in a fresh process.
        await asyncio.sleep(0.6)
        assert not worker.alive
        report, thread = await worker.run("Once more", "thr_1")
        assert thread == "thr_1"
        assert fake_worker_cli()[-1]["pid"] != first_pid
        assert [entry["kind"] for entry in fake_worker_cli() if entry["kind"] in {"thread/start", "thread/resume"}] == ["thread/start", "thread/resume"]
        await worker.close()
        assert not worker.alive

    asyncio.run(scenario())


def test_codex_worker_falls_back_when_thread_is_lost(heavy_config, fake_worker_cli, monkeypatch):
    monkeypatch.setenv("LOST_THREAD", "thr_gone")
    monkeypatch.setenv("THREAD_SUFFIX", "new")
    config = replace(heavy_config, backend="codex")

    async def scenario():
        worker = CodexWorker(config)
        report, thread = await worker.run("Run", "thr_gone")
        assert thread == "thr_new" and report.startswith("Ran the suite")
        await worker.close()

    asyncio.run(scenario())
    kinds = [entry["kind"] for entry in fake_worker_cli()]
    assert kinds.index("thread/resume") < kinds.index("thread/start")


def test_codex_worker_declines_approvals(heavy_config, fake_worker_cli, monkeypatch, caplog):
    monkeypatch.setenv("ASK_APPROVAL", "1")
    config = replace(heavy_config, backend="codex")

    async def scenario():
        worker = CodexWorker(config)
        report, _thread = await worker.run("Clean the build", None)
        assert report.startswith("Ran the suite")
        await worker.close()

    import logging
    with caplog.at_level(logging.WARNING, logger="fridica.worker"):
        asyncio.run(scenario())
    assert "requestApproval" in caplog.text and "declined" in caplog.text
    assert any(entry["kind"] == "approval-answer" for entry in fake_worker_cli())


@pytest.mark.parametrize("prompt,match", [("FAIL now", "model refused"), ("EXIT now", "exited"), ("HANG", "")])
def test_codex_worker_failures_close_the_process(heavy_config, fake_worker_cli, prompt, match):
    config = replace(heavy_config, backend="codex", heavy_task_timeout=1)

    async def scenario():
        worker = CodexWorker(config)
        expected = TimeoutError if prompt == "HANG" else BackendError
        with pytest.raises(expected) as info:
            await worker.run(prompt, None)
        if match:
            assert match in str(info.value)
        assert not worker.alive
        await worker.close()

    asyncio.run(scenario())


def test_claude_worker_roundtrip(heavy_config, fake_worker_cli):
    config = replace(heavy_config, backend="claude")

    async def scenario():
        worker = ClaudeWorker(config)
        report, session = await worker.run("Run the suite", None)
        assert report == "Job finished: all green." and session
        argv = fake_worker_cli()[0]["argv"]
        assert argv[argv.index("--session-id") + 1] == session and "--resume" not in argv
        assert fake_worker_cli()[0]["omp"] == "4"
        settings = json.loads(argv[argv.index("--settings") + 1])
        assert settings["sandbox"]["enabled"] and settings["sandbox"]["failIfUnavailable"]
        assert argv[argv.index("--tools") + 1] == "Bash,Read,Glob,Grep,Edit,Write"
        report, again = await worker.run("Again", session)
        assert again == session and len(fake_worker_cli()) == 1
        await asyncio.sleep(0.6)
        assert not worker.alive
        report, resumed = await worker.run("Once more", session)
        assert resumed == session
        argv = fake_worker_cli()[-1]["argv"]
        assert argv[argv.index("--resume") + 1] == session and "--session-id" not in argv
        with pytest.raises(BackendError) as info:
            await worker.run("FAIL", session)
        assert "PRIVATE FAILURE" in str(info.value) and not worker.alive
        await worker.close()

    asyncio.run(scenario())


def test_workers_registry_closes_everything(heavy_config, fake_worker_cli):
    config = replace(heavy_config, backend="codex")

    async def scenario():
        workers = Workers(config, CodexWorker)
        first, second = workers.get("task-a"), workers.get("task-b")
        assert workers.get("task-a") is first and first is not second
        await first.run("Run", None)
        await second.run("Run", None)
        assert first.alive and second.alive
        await workers.close()
        assert not first.alive and not second.alive and workers.workers == {}

    asyncio.run(scenario())


def test_codex_worker_over_ssh(heavy_config, fake_worker_cli, tmp_path, monkeypatch):
    from test_remote import FAKE_SSH
    ssh = tmp_path / "bin" / "ssh"
    ssh.write_text(FAKE_SSH)
    ssh.chmod(0o700)
    monkeypatch.setenv("SSH_LOG", str(tmp_path / "ssh.log"))
    monkeypatch.setenv("EXPECTED_HOST", "dart9")
    config = replace(heavy_config, backend="codex", ssh_host="dart9", workspace=PurePosixPath(heavy_config.workspace))

    async def scenario():
        worker = CodexWorker(config)
        report, thread = await worker.run("Run", None)
        assert report.startswith("Ran the suite") and thread == "thr_1"
        await worker.close()

    asyncio.run(scenario())
    entry = fake_worker_cli()[0]
    assert entry["cwd"] == str(config.workspace) and entry["omp"] == "4" and entry["cuda"] == "0,1"
    call = json.loads((tmp_path / "ssh.log").read_text().splitlines()[0])
    assert call["host"] == "dart9" and "codex app-server" in call["script"] and "export OMP_NUM_THREADS=4" in call["script"]


def test_claude_worker_recovers_from_lost_session(heavy_config, fake_worker_cli, tmp_path, monkeypatch):
    """A stale session id is retried once on a fresh session instead of failing every later job."""
    executable = tmp_path / "bin" / "claude"
    executable.write_text(FAKE_CLAUDE_STREAM.replace(
        'sys.stdout.write(json.dumps({"type": "system"',
        'if session == "lost-session":\n'
        '    print("No conversation found with session ID: lost-session", file=sys.stderr)\n'
        '    sys.exit(1)\n'
        'sys.stdout.write(json.dumps({"type": "system"'))
    config = replace(heavy_config, backend="claude")

    async def scenario():
        worker = ClaudeWorker(config)
        report, session = await worker.run("Run", "lost-session")
        assert report == "Job finished: all green." and session != "lost-session"
        await worker.close()

    asyncio.run(scenario())
    first, second = fake_worker_cli()
    assert first["argv"][first["argv"].index("--resume") + 1] == "lost-session"
    assert "--session-id" in second["argv"] and "--resume" not in second["argv"]


def test_workers_close_leaves_busy_worker_to_finish(heavy_config, fake_worker_cli):
    config = replace(heavy_config, backend="codex", heavy_task_timeout=5)

    async def scenario():
        workers = Workers(config, CodexWorker)
        idle, busy = workers.get("idle"), workers.get("busy")
        await idle.run("Run", None)
        job = asyncio.create_task(busy.run("HANG a little", None))
        for _ in range(100):
            if busy.busy.locked() and busy.alive:
                break
            await asyncio.sleep(0.01)
        await workers.close()
        assert not idle.alive and busy.alive and workers.workers == {}
        job.cancel()
        with pytest.raises(asyncio.CancelledError):
            await job
        assert not busy.alive

    asyncio.run(scenario())


def test_idle_close_skips_a_worker_that_became_busy(heavy_config, fake_worker_cli):
    config = replace(heavy_config, backend="codex", heavy_task_idle=0.05)

    async def scenario():
        worker = CodexWorker(config)
        await worker.run("Run", None)
        # Claim the worker before the idle timer fires; the timer must not touch the process.
        async with worker.busy:
            await asyncio.sleep(0.15)
            assert worker.alive
        # A stale timer handle never closes a worker that has since been re-armed.
        stale = worker.idle_timer
        worker._schedule_idle()
        await worker._idle_close(stale)
        assert worker.alive
        await asyncio.sleep(0.15)
        assert not worker.alive
        await worker.close()

    asyncio.run(scenario())
