import asyncio
import json
from pathlib import Path, PurePosixPath

import pytest

from fridica.core.errors import BackendError
from fridica.machines.registry import Machine, Policy, Resources, Workspace
from fridica.workers.claude import ClaudeWorker
from fridica.workers.codex import CodexWorker
from fridica.workers.protocol import ALLOW_ONCE, ALLOW_SESSION, DENY, WorkerSpec
from fridica.workers.result import RESULT_SCHEMA


def spec(tmp_path, backend="codex", *, policy=Policy(network=("github.com",)), transport="local", resources=Resources(cpus=4),
         idle=30.0, timeout=20.0):
    work = tmp_path / "work"
    work.mkdir(exist_ok=True)
    path = work if transport == "local" else PurePosixPath(work)
    machine = Machine(name="box", transport=transport, workspaces=(Workspace("work", path, policy),),
                      backends=("codex", "claude"), default_backend=backend, policy=policy,
                      host="" if transport == "local" else "box", resources=resources)
    return WorkerSpec("w1", machine, machine.workspaces[0], backend, instructions="Speak as the owner.",
                      job_timeout=timeout, idle_timeout=idle, excluded_env=("SLACK_USER_TOKEN",))


def run(coroutine):
    return asyncio.run(coroutine)


class Handler:
    def __init__(self, *decisions):
        self.decisions = list(decisions)
        self.requests = []

    async def __call__(self, request):
        self.requests.append(request)
        return self.decisions.pop(0)


# ----- codex -----

def test_codex_job_returns_a_structured_result_and_maps_policy(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            return await worker.run("Build it")
        finally:
            await worker.close()

    outcome = run(scenario())
    assert outcome.result.status == "done" and outcome.result.report == "All green."
    assert outcome.result.validation[0].outcome == "passed" and outcome.result.machine_state.dirty
    assert outcome.backend_session_id.startswith("thr_")
    start = fake_agents("thread/start")[0]
    params = start["data"]["params"]
    assert params["approvalPolicy"] == "on-request" and params["approvalsReviewer"] == "auto_review"  # auto by default
    assert params["sandbox"] == "workspace-write"
    assert params["developerInstructions"] == "Speak as the owner." and params["cwd"] == str(tmp_path / "work")
    turn = fake_agents("turn/start")[0]["data"]["params"]
    assert turn["outputSchema"] == RESULT_SCHEMA
    assert turn["sandboxPolicy"] == {"type": "workspaceWrite", "networkAccess": True, "writableRoots": []}
    assert "Build it" in turn["input"][0]["text"]
    assert start["omp"] == "4" and start["slack"] is None and start["cwd"] == str(tmp_path / "work")
    assert "sandbox_workspace_write.network_access=true" in start["argv"]


def test_codex_follow_up_reuses_the_process_and_a_new_process_resumes_the_thread(tmp_path, fake_agents, monkeypatch):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        first = await worker.run("one")
        second = await worker.run("two", resume=first.backend_session_id)
        await worker.close()
        third = await worker.run("three", resume=first.backend_session_id)
        await worker.close()
        return first, second, third

    first, second, third = run(scenario())
    assert first.backend_session_id == second.backend_session_id == third.backend_session_id
    assert len({row["pid"] for row in fake_agents("turn/start")}) == 2
    assert fake_agents("thread/resume")[0]["data"]["params"]["threadId"] == first.backend_session_id


def test_codex_lost_thread_starts_a_new_one(tmp_path, fake_agents, monkeypatch):
    monkeypatch.setenv("LOST_THREAD", "thr_gone")

    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            return await worker.run("again", resume="thr_gone")
        finally:
            await worker.close()

    outcome = run(scenario())
    assert outcome.backend_session_id not in ("thr_gone", "") and outcome.result.status == "done"
    assert len(fake_agents("thread/start")) == 1


@pytest.mark.parametrize("decision, answer", [(ALLOW_ONCE, "accept"), (ALLOW_SESSION, "acceptForSession"), (DENY, "decline")])
def test_codex_approvals_go_to_the_handler(tmp_path, fake_agents, decision, answer):
    handler = Handler(decision)

    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            return await worker.run("APPROVE:make-install please", on_approval=handler)
        finally:
            await worker.close()

    outcome = run(scenario())
    assert handler.requests[0].kind == "command" and "make-install" in handler.requests[0].summary
    assert fake_agents("approval-answer")[0]["data"]["result"] == {"decision": answer}
    assert outcome.result.summary == f"approval {answer}"


def test_allow_for_session_is_remembered_by_the_worker(tmp_path, fake_agents):
    handler = Handler(ALLOW_SESSION)

    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            await worker.run("APPROVE:rm-build", on_approval=handler)
            return await worker.run("APPROVE:rm-build", on_approval=handler)
        finally:
            await worker.close()

    run(scenario())
    assert len(handler.requests) == 1
    assert [row["data"]["result"]["decision"] for row in fake_agents("approval-answer")] == ["acceptForSession"] * 2


def test_codex_permission_requests_and_unsupported_requests(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            granted = await worker.run("PERMS", on_approval=Handler(ALLOW_ONCE))
            await worker.run("ELICIT")
            return granted
        finally:
            await worker.close()

    outcome = run(scenario())
    assert json.loads(outcome.result.summary[len("perms "):]) == {"permissions": {"network": {"enabled": True}}, "scope": "turn"}
    assert "error" in fake_agents("elicit-answer")[0]["data"]


def test_codex_interrupt_stops_the_turn(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        job = asyncio.create_task(worker.run("HANG"))
        for _ in range(100):
            await asyncio.sleep(0.05)
            if worker.turn_id:
                break
        await worker.interrupt()
        with pytest.raises(BackendError, match="interrupted"):
            await job
        return worker

    worker = run(scenario())
    assert not worker.alive
    assert fake_agents("turn/interrupt")[0]["data"]["params"]["turnId"] == "turn_1"


def test_codex_prose_is_followed_by_a_summarize_turn(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        try:
            return await worker.run("PROSE")
        finally:
            await worker.close()

    outcome = run(scenario())
    assert outcome.result.summary == "summarized after prose"
    assert len(fake_agents("turn/start")) == 2


@pytest.mark.parametrize("prompt, message", [("FAIL", "model refused"), ("EXIT", "status 3")])
def test_codex_failures_raise_and_close(tmp_path, fake_agents, prompt, message):
    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        with pytest.raises(BackendError, match=message):
            await worker.run(prompt)
        return worker

    assert not run(scenario()).alive


def test_codex_read_only_and_gpu_confined_policies(tmp_path, fake_agents, monkeypatch):
    read_only = CodexWorker(spec(tmp_path, policy=Policy(mode="read-only")))
    assert read_only.sandbox_mode() == "read-only"
    assert read_only.sandbox_policy() == {"type": "readOnly", "networkAccess": False}
    gpu = spec(tmp_path, policy=Policy(gpu_confine=True), resources=Resources(gpus=(0,)))

    async def scenario():
        worker = CodexWorker(gpu)
        try:
            return await worker.run("go")
        finally:
            await worker.close()

    run(scenario())
    assert fake_agents("turn/start")[0]["data"]["params"]["sandboxPolicy"] == {"type": "dangerFullAccess"}
    bwrap = json.loads((tmp_path / "bwrap.log").read_text().splitlines()[0])
    assert ["--bind", str(tmp_path / "work"), str(tmp_path / "work")] == bwrap[bwrap.index(str(tmp_path / "work")) - 1:][:3]


def test_codex_worker_over_ssh(tmp_path, fake_agents, fake_ssh):
    async def scenario():
        worker = CodexWorker(spec(tmp_path, transport="ssh"))
        try:
            return await worker.run("remote job")
        finally:
            await worker.close()

    outcome = run(scenario())
    assert f"cwd={tmp_path / 'work'}" in outcome.result.summary
    assert json.loads(fake_ssh.read_text().splitlines()[0])["host"] == "box"


def test_idle_worker_closes_itself(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path, idle=0.2))
        await worker.run("go")
        assert worker.alive
        await asyncio.sleep(0.6)
        return worker.alive

    assert run(scenario()) is False


# ----- claude -----

def test_claude_job_session_and_command_shape(tmp_path, fake_agents):
    async def scenario():
        worker = ClaudeWorker(spec(tmp_path, "claude"))
        first = await worker.run("Fix it")
        second = await worker.run("And test", resume=first.backend_session_id)
        await worker.close()
        third = await worker.run("Again", resume=first.backend_session_id)
        await worker.close()
        return first, second, third

    first, second, third = run(scenario())
    assert first.result.report == "All green on claude." and first.result.machine_state.branch == "dev"
    assert first.backend_session_id == second.backend_session_id == third.backend_session_id
    users = fake_agents("user")
    assert len({row["pid"] for row in users}) == 2
    argv = users[0]["argv"]
    assert argv[argv.index("--permission-mode") + 1] == "auto"  # the default approvals mode
    assert argv[argv.index("--append-system-prompt") + 1] == "Speak as the owner."
    assert "--permission-prompt-tool" in argv and "--session-id" in argv
    assert "--resume" in users[-1]["argv"]
    settings = json.loads(argv[argv.index("--settings") + 1])
    assert settings["sandbox"]["network"]["allowedDomains"] == ["github.com"] and settings["disableAllHooks"]
    assert "WorkerResult" in users[0]["data"]["message"]["content"][0]["text"]
    assert fake_agents("control_request")[0]["data"]["request"]["subtype"] == "initialize"


@pytest.mark.parametrize("decision, behavior", [(ALLOW_ONCE, "allow"), (DENY, "deny")])
def test_claude_tool_approvals_use_the_control_protocol(tmp_path, fake_agents, decision, behavior):
    handler = Handler(decision)

    async def scenario():
        worker = ClaudeWorker(spec(tmp_path, "claude"))
        try:
            return await worker.run("TOOL:nvidia-smi", on_approval=handler)
        finally:
            await worker.close()

    outcome = run(scenario())
    assert handler.requests[0].summary == "Bash: nvidia-smi"
    answer = fake_agents("approval-answer")[0]["data"]
    assert answer["response"]["request_id"] == "perm-1" and answer["response"]["response"]["behavior"] == behavior
    assert outcome.result.summary == f"tool {behavior}"


def test_claude_without_approvals_denies_prompts_outright(tmp_path):
    worker = ClaudeWorker(spec(tmp_path, "claude", policy=Policy(approvals="never")))
    worker.prepare("")
    argv = worker.command()
    assert argv[argv.index("--permission-prompts") + 1] == "none" and "--permission-prompt-tool" not in argv
    read_only = ClaudeWorker(spec(tmp_path, "claude", policy=Policy(mode="read-only")))
    read_only.prepare("")
    argv = read_only.command()
    assert argv[argv.index("--tools") + 1] == "Read,Glob,Grep"


def test_claude_lost_session_retries_fresh(tmp_path, fake_agents, monkeypatch):
    monkeypatch.setenv("LOST_SESSION", "gone-session")

    async def scenario():
        worker = ClaudeWorker(spec(tmp_path, "claude"))
        try:
            return await worker.run("hello", resume="gone-session")
        finally:
            await worker.close()

    outcome = run(scenario())
    assert outcome.backend_session_id != "gone-session" and outcome.result.status == "done"


def test_claude_interrupt_and_failure(tmp_path, fake_agents):
    async def scenario():
        worker = ClaudeWorker(spec(tmp_path, "claude"))
        job = asyncio.create_task(worker.run("HANG"))
        for _ in range(100):
            await asyncio.sleep(0.05)
            if worker.in_turn:
                break
        await worker.interrupt()
        with pytest.raises(BackendError, match="interrupted"):
            await job
        with pytest.raises(BackendError, match="PRIVATE FAILURE"):
            await worker.run("FAIL")

    run(scenario())
    assert "interrupt" in [row["data"]["request"]["subtype"] for row in fake_agents("control_request")]


def test_workspace_path_types(tmp_path):
    assert isinstance(spec(tmp_path).workspace.path, Path)
    assert isinstance(spec(tmp_path, transport="ssh").workspace.path, PurePosixPath)


def test_interrupt_while_an_approval_is_pending_denies_it(tmp_path, fake_agents):
    cancelled = []

    async def never(request):
        try:
            await asyncio.sleep(3600)
        except asyncio.CancelledError:
            cancelled.append(request.summary)
            raise

    async def scenario():
        worker = CodexWorker(spec(tmp_path))
        job = asyncio.create_task(worker.run("APPROVE:make-install", on_approval=never))
        for _ in range(100):
            await asyncio.sleep(0.05)
            if fake_agents("turn/start"):
                break
        await asyncio.sleep(0.1)
        await worker.interrupt()
        with pytest.raises(BackendError, match="interrupted"):
            await asyncio.wait_for(job, 10)
        await worker.close()

    run(scenario())
    assert cancelled == ["Run `make-install`"]
    assert fake_agents("approval-answer")[0]["data"]["result"] == {"decision": "decline"}


def test_codex_interrupt_before_the_turn_id_is_known_is_sent_later(tmp_path):
    worker = CodexWorker(spec(tmp_path))
    sent = []

    async def scenario():
        worker.process = type("P", (), {"returncode": None, "stdin": None})()
        worker._lock = asyncio.Lock()
        await worker._lock.acquire()
        await worker.interrupt()
        assert worker.pending_interrupt and sent == []

        async def send(message):
            sent.append(message)
        worker.send = send
        worker.turn_id = "turn_7"
        worker.session = "thr"
        await worker._send_interrupt()

    run(scenario())
    assert sent == [{"id": 1, "method": "turn/interrupt", "params": {"threadId": "thr", "turnId": "turn_7"}}]


def test_auto_approvals_use_the_backends_own_reviewers(tmp_path, fake_agents):
    async def scenario():
        worker = CodexWorker(spec(tmp_path, policy=Policy(approvals="auto")))
        try:
            return await worker.run("go")
        finally:
            await worker.close()

    run(scenario())
    params = fake_agents("thread/start")[0]["data"]["params"]
    assert params["approvalPolicy"] == "on-request" and params["approvalsReviewer"] == "auto_review"
    claude = ClaudeWorker(spec(tmp_path, "claude", policy=Policy(approvals="auto")))
    claude.prepare("")
    argv = claude.command()
    assert argv[argv.index("--permission-mode") + 1] == "auto" and "--permission-prompt-tool" in argv
    assert Policy().approvals == "auto"
    asking = CodexWorker(spec(tmp_path, policy=Policy(approvals="on-request")))
    asking.prepare("")
    assert asking.policy.approvals == "on-request"


def test_claude_warns_when_auto_mode_falls_back(tmp_path, fake_agents, monkeypatch, caplog):
    monkeypatch.setenv("FAKE_PERMISSION_MODE", "default")

    async def scenario():
        worker = ClaudeWorker(spec(tmp_path, "claude", policy=Policy(approvals="auto")))
        try:
            return await worker.run("go")
        finally:
            await worker.close()

    import logging
    with caplog.at_level(logging.WARNING, logger="fridica.workers.claude"):
        outcome = run(scenario())
    assert outcome.result.status == "done"
    assert "runs in default mode instead of auto" in caplog.text


def test_on_request_keeps_claude_in_accept_edits(tmp_path):
    worker = ClaudeWorker(spec(tmp_path, "claude", policy=Policy(approvals="on-request")))
    worker.prepare("")
    argv = worker.command()
    assert argv[argv.index("--permission-mode") + 1] == "acceptEdits" and "--permission-prompt-tool" in argv
