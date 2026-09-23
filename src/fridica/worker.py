"""Persistent heavy-task workers: one long-lived agent process per Slack thread.

A per-turn reply is one short CLI run. When the reply agent escalates a request, the
thread gets a worker instead: ``codex app-server`` speaking JSON-RPC over JSONL on
stdin/stdout, or ``claude -p --input-format stream-json --output-format stream-json``,
started on the working host (locally, or through ``ssh -T`` when the workspace is
remote). The process stays alive between jobs until it has been idle for
``heavy_task_idle`` seconds; the backend thread it created is remembered so a later
job resumes it in a fresh process. Approval requests from the agent are declined:
Fridica has no approval UI, so the worker runs with the same sandbox and
approval-free policy as per-turn task runs.
"""
from __future__ import annotations

import asyncio
import json
import logging
from typing import Any
import uuid

from . import remote
from .config import Config, Host
from .runner import OUTPUT_LIMIT, RESUME_FAILURES, BackendError, _terminate, diagnostic, environment

logger = logging.getLogger(__name__)
STDERR_LIMIT = 64 * 1024
CLIENT_INFO = {"name": "fridica", "title": "Fridica", "version": "1"}
DECLINE = {"decision": "decline"}


def bounded(text: object, limit: int = OUTPUT_LIMIT) -> str:
    if not isinstance(text, str) or not text.strip():
        raise BackendError("Worker returned an empty report.")
    return text if len(text) <= limit else text[:limit]


class Worker:
    """A long-lived agent process bound to one Slack thread; subclasses speak its protocol.

    ``run`` is the only entry point. It (re)starts the process when needed, performs the
    protocol handshake, runs one job bounded by ``heavy_task_timeout``, and arms the
    idle timer that closes the process after ``heavy_task_idle`` seconds.
    """

    def __init__(self, config: Config, host: Host | None = None):
        self.config = config
        self.host = host or config.primary
        self.process: asyncio.subprocess.Process | None = None
        self.reader: asyncio.Task | None = None
        self.stderr_task: asyncio.Task | None = None
        self.lines: asyncio.Queue[str | None] = asyncio.Queue()
        self.stderr = bytearray()
        self.idle_timer: asyncio.TimerHandle | None = None
        self.thread: str | None = None
        self.resume: str | None = None
        self.busy = asyncio.Lock()

    # ----- process lifecycle -----

    def command(self) -> list[str]:
        """The agent argv, built after ``prepare`` so it can name the session to resume."""
        raise NotImplementedError

    @property
    def alive(self) -> bool:
        return self.process is not None and self.process.returncode is None

    async def start(self) -> None:
        limits = self.host.resources.environment()
        confine = self.host.roots if self.host.resources.gpu_worker else None
        argv, cwd = remote.launch(self.host, self.command(), self.host.workspace, env=limits, confine=confine)
        env = environment(self.config)
        if not self.host.remote:
            env.update(limits)
        try:
            self.process = await asyncio.create_subprocess_exec(
                *argv, cwd=cwd, env=env, limit=OUTPUT_LIMIT, stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
        except OSError as error:
            raise BackendError("Could not start the heavy-task worker executable.") from error
        self.lines = asyncio.Queue()
        self.stderr = bytearray()
        self.reader = asyncio.create_task(self._read_lines())
        self.stderr_task = asyncio.create_task(self._read_stderr())

    async def _read_lines(self) -> None:
        assert self.process is not None and self.process.stdout is not None
        try:
            while True:
                try:
                    line = await self.process.stdout.readline()
                except ValueError:
                    logger.error("Heavy-task worker emitted a line beyond the size limit; stopping it")
                    break
                if not line:
                    break
                self.lines.put_nowait(line.decode("utf-8", "replace"))
        finally:
            self.lines.put_nowait(None)

    async def _read_stderr(self) -> None:
        assert self.process is not None and self.process.stderr is not None
        while chunk := await self.process.stderr.read(4096):
            self.stderr.extend(chunk)
            del self.stderr[:-STDERR_LIMIT]

    async def close(self) -> None:
        """Stop the process and its readers; safe to call repeatedly.

        Everything belonging to the current process is detached before the first
        ``await`` so a ``run`` that starts a new process meanwhile is left alone.
        """
        self._cancel_idle()
        process, self.process = self.process, None
        tasks, self.reader, self.stderr_task = (self.reader, self.stderr_task), None, None
        if process is not None and process.returncode is None:
            if process.stdin is not None:
                process.stdin.close()
            await _terminate(process)
        for task in tasks:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)

    def _cancel_idle(self) -> None:
        if self.idle_timer is not None:
            self.idle_timer.cancel()
            self.idle_timer = None

    def _schedule_idle(self) -> None:
        self._cancel_idle()
        loop = asyncio.get_running_loop()
        self.idle_timer = handle = loop.call_later(
            self.config.heavy_task_idle, lambda: loop.create_task(self._idle_close(handle)))

    async def _idle_close(self, handle: asyncio.TimerHandle) -> None:
        """Close after the idle period unless a job claimed the worker (and re-armed the timer) meanwhile."""
        if self.busy.locked() or self.idle_timer is not handle:
            return
        async with self.busy:
            if self.idle_timer is handle:
                await self.close()

    # ----- protocol helpers -----

    async def send(self, message: dict) -> None:
        if not self.alive or self.process.stdin is None:
            raise BackendError("Heavy-task worker process is not running.")
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        await self.process.stdin.drain()

    async def receive(self) -> dict:
        """The next JSON object from the worker; raises when the process ended."""
        while True:
            line = await self.lines.get()
            if line is None:
                status = self.process.returncode if self.process is not None else None
                raise BackendError(f"Heavy-task worker exited (status {status}). {diagnostic(bytes(self.stderr))}".strip())
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if isinstance(message, dict):
                return message

    # ----- the one operation callers use -----

    async def run(self, prompt: str, resume: str | None) -> tuple[str, str | None]:
        """Run one job and return ``(report, thread_id)``; ``resume`` is an earlier job's thread id.

        When the backend reports that the thread to resume no longer exists, the job is
        retried once on a fresh thread so a stale identifier cannot block a Slack thread.
        """
        async with self.busy:
            self._cancel_idle()
            try:
                try:
                    report = await self._attempt(prompt, resume)
                except BackendError as error:
                    if resume is None or not any(marker in str(error) for marker in RESUME_FAILURES):
                        raise
                    logger.info("Heavy-task worker could not resume %s; starting a new thread", resume)
                    report = await self._attempt(prompt, None)
            except BaseException:
                await self.close()
                raise
            self._schedule_idle()
            return bounded(report), self.thread

    async def _attempt(self, prompt: str, resume: str | None) -> str:
        if not self.alive or (resume is not None and self.thread != resume):
            await self.close()
            self.prepare(resume)
            await self.start()
            await self.handshake()
        return await asyncio.wait_for(self.job(prompt), self.config.heavy_task_timeout)

    def prepare(self, resume: str | None) -> None:
        """Remember which backend thread the next process should continue."""
        self.resume = resume
        self.thread = resume

    async def handshake(self) -> None:
        """Protocol setup after the process started; nothing by default."""

    async def job(self, prompt: str) -> str:
        raise NotImplementedError


class CodexWorker(Worker):
    """``codex app-server`` over stdio: initialize, thread/start or thread/resume, then one turn per job."""

    def __init__(self, config: Config, host: Host | None = None):
        super().__init__(config, host)
        self.next_id = 1

    def command(self) -> list[str]:
        # Unlike ``codex exec``, ``codex app-server`` accepts neither --ignore-user-config nor
        # --ignore-rules, and ``-c mcp_servers={}`` merges rather than clears, so the owner's
        # ~/.codex/config.toml (including its MCP servers) applies to heavy jobs on top of
        # the feature switches below. The README says so.
        from .agents import CodexBackend
        settings = [*CodexBackend.FEATURES_OFF, "features.code_mode_host=true",
                    "sandbox_workspace_write.network_access=" + ("true" if self.config.allowed_domains else "false")]
        if self.config.reasoning_effort:
            settings.append("model_reasoning_effort=" + json.dumps(self.config.reasoning_effort))
        command = ["codex", "app-server"]
        for setting in settings:
            command += ["-c", setting]
        return command

    async def request(self, method: str, params: dict) -> dict:
        identifier = self.next_id
        self.next_id += 1
        await self.send({"id": identifier, "method": method, "params": params})
        while True:
            message = await self.receive()
            if message.get("id") == identifier and "method" not in message:
                if "error" in message:
                    detail = message["error"].get("message") if isinstance(message["error"], dict) else message["error"]
                    raise BackendError(f"{method} failed: {' '.join(str(detail).split())[:500]}")
                result = message.get("result")
                return result if isinstance(result, dict) else {}
            await self.dispatch(message)

    async def dispatch(self, message: dict) -> None:
        """Handle a message that is not the response being waited for."""
        if "method" in message and "id" in message:
            # A server request (approval, elicitation): Fridica cannot ask anyone, so decline.
            logger.warning("Heavy-task worker asked for %s; declined", message["method"])
            await self.send({"id": message["id"], "result": DECLINE})

    async def handshake(self) -> None:
        self.next_id = 1
        await self.request("initialize", {"clientInfo": CLIENT_INFO, "capabilities": {"experimentalApi": False}})
        await self.send({"method": "initialized", "params": {}})
        # A GPU worker is already confined by Fridica's bubblewrap; Codex's own sandbox would hide the GPUs.
        sandbox = "danger-full-access" if self.host.resources.gpu_worker else "workspace-write"
        params = {"cwd": str(self.host.workspace), "sandbox": sandbox, "approvalPolicy": "never"}
        if self.config.model:
            params["model"] = self.config.model
        thread = None
        if self.resume is not None:
            try:
                thread = (await self.request("thread/resume", {"threadId": self.resume, **params})).get("thread")
            except BackendError as error:
                logger.info("Heavy-task worker could not resume thread %s (%s); starting a new one", self.resume, error)
        if not isinstance(thread, dict) or not isinstance(thread.get("id"), str):
            thread = (await self.request("thread/start", {**params, "ephemeral": False})).get("thread")
        if not isinstance(thread, dict) or not isinstance(thread.get("id"), str):
            raise BackendError("codex app-server did not return a thread.")
        self.thread = thread["id"]

    async def job(self, prompt: str) -> str:
        if self.host.resources.gpu_worker:
            policy: dict[str, Any] = {"type": "dangerFullAccess"}
        else:
            policy = {"type": "workspaceWrite", "networkAccess": bool(self.config.allowed_domains),
                      "writableRoots": [str(root) for root in self.host.roots[1:]]}
        result = await self.request("turn/start", {"threadId": self.thread, "input": [{"type": "text", "text": prompt}],
                                                   "cwd": str(self.host.workspace), "sandboxPolicy": policy})
        turn = result.get("turn") if isinstance(result.get("turn"), dict) else {}
        turn_id = turn.get("id")
        report = None
        while True:
            message = await self.receive()
            method = message.get("method")
            params = message.get("params") if isinstance(message.get("params"), dict) else {}
            if method is None:
                continue
            if "id" in message:
                await self.dispatch(message)
                continue
            mine = turn_id is None or params.get("turnId") == turn_id or (params.get("turn") or {}).get("id") == turn_id
            if method == "item/completed" and mine:
                item = params.get("item") if isinstance(params.get("item"), dict) else {}
                if item.get("type") == "agentMessage" and isinstance(item.get("text"), str):
                    report = item["text"]
            elif method == "turn/completed" and mine:
                completed = params.get("turn") if isinstance(params.get("turn"), dict) else {}
                if completed.get("status") != "completed":
                    error = completed.get("error") if isinstance(completed.get("error"), dict) else {}
                    detail = " ".join(str(error.get("message", completed.get("status"))).split())[:500]
                    raise BackendError(f"Heavy task {completed.get('status', 'failed')}: {detail}")
                for item in (completed.get("items") or []) if report is None else []:
                    if isinstance(item, dict) and item.get("type") == "agentMessage" and isinstance(item.get("text"), str):
                        report = item["text"]
                return report or ""
            elif method == "error" and mine and params.get("willRetry") is False:
                error = params.get("error") if isinstance(params.get("error"), dict) else {}
                raise BackendError("Heavy task failed: " + " ".join(str(error.get("message", "unknown error")).split())[:500])


class ClaudeWorker(Worker):
    """``claude -p`` with streaming JSON on both ends: one user message in, one result out, per job."""

    def command(self) -> list[str]:
        from .agents import ClaudeBackend
        settings = ClaudeBackend.settings(self.config, False)
        allowed = "Read,Glob,Grep"
        if self.host.resources.gpu_worker:
            # Claude's sandbox hides the GPU devices, so it is off for GPU work and Fridica's
            # bubblewrap confines the process instead; Bash then needs an explicit allowance.
            settings["sandbox"] = {"enabled": False}
            allowed = "Bash," + allowed
        command = ["claude", "-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose",
                   "--setting-sources", "", "--settings", json.dumps(settings),
                   "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}', "--disable-slash-commands", "--no-chrome",
                   "--permission-mode", "acceptEdits", "--tools", "Bash,Read,Glob,Grep,Edit,Write",
                   "--allowedTools", allowed]
        command += ["--resume", self.resume] if self.resume is not None else ["--session-id", self.thread]
        for workspace in self.host.roots[1:]:
            command += ["--add-dir", str(workspace)]
        if self.config.model:
            command += ["--model", self.config.model]
        return command

    def prepare(self, resume: str | None) -> None:
        # The session is fixed on the command line: --resume continues an earlier one, --session-id names a new one.
        self.resume = resume
        self.thread = resume or str(uuid.uuid4())

    async def job(self, prompt: str) -> str:
        await self.send({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": prompt}]}})
        while True:
            message = await self.receive()
            if isinstance(message.get("session_id"), str) and message.get("type") in {"system", "result"}:
                self.thread = message["session_id"]
            if message.get("type") == "result":
                if message.get("is_error"):
                    detail = " ".join(str(message.get("result") or message.get("subtype") or "error").split())[:500]
                    raise BackendError(f"Heavy task failed: {detail}")
                result = message.get("result")
                return result if isinstance(result, str) else ""


class Workers:
    """The heavy-task workers of one backend, keyed by Slack task and host."""

    def __init__(self, config: Config, factory):
        self.config = config
        self.factory = factory
        self.workers: dict[tuple[str, str], Worker] = {}

    def get(self, task_id: str, host: Host | None = None) -> Worker:
        host = host or self.config.primary
        worker = self.workers.get((task_id, host.name))
        if worker is None:
            worker = self.workers[(task_id, host.name)] = self.factory(self.config, host)
        return worker

    async def close(self) -> None:
        """Stop idle workers and forget them all.

        A worker in the middle of a job is left to finish: its own idle timer closes it
        afterwards, and the job's cancellation (on shutdown) closes it at once.
        """
        workers, self.workers = list(self.workers.values()), {}
        await asyncio.gather(*(worker.close() for worker in workers if not worker.busy.locked()), return_exceptions=True)
