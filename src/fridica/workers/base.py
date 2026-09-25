"""A long-lived agent process speaking JSONL on stdio; backends subclass it with their protocol.

``run`` (re)starts the process when needed, performs the handshake, runs one job
bounded by the job timeout, makes sure a WorkerResult comes back, and arms an idle
timer that closes the process when no follow-up arrives. The backend session id is
kept so a later job, even in a fresh process, continues the same conversation.
"""

from __future__ import annotations

import asyncio
import json
import logging

from ..core.errors import BackendError, SessionUnavailable
from ..exec.process import OUTPUT_LIMIT, diagnostic, release, terminate
from ..exec.transport import Transport, make_transport
from .protocol import ALLOW_SESSION, DENY, ApprovalHandler, ApprovalRequest, Outcome, WorkerSpec, deny_all
from .result import SUMMARIZE_PROMPT, fallback, parse

logger = logging.getLogger(__name__)
STDERR_LIMIT = 64 * 1024
RESUME_FAILURES = ("No conversation found with session ID", "no rollout found for thread id")


class JsonlWorker:
    backend = ""

    def __init__(self, spec: WorkerSpec, transport: Transport | None = None):
        self.spec = spec
        self.transport = transport or make_transport(spec.machine, excluded_env=spec.excluded_env)
        self.process: asyncio.subprocess.Process | None = None
        self.reader: asyncio.Task | None = None
        self.stderr_task: asyncio.Task | None = None
        self.lines: asyncio.Queue[str | None] = asyncio.Queue()
        self.stderr = bytearray()
        self.idle_timer: asyncio.TimerHandle | None = None
        self.session = ""
        self.resume = ""
        self._lock = asyncio.Lock()
        self._approved: set[str] = set()
        self._on_approval: ApprovalHandler = deny_all
        self._interrupted = asyncio.Event()
        self._idle_task: asyncio.Task | None = None

    # ----- backend hooks -----

    def command(self) -> list[str]:
        raise NotImplementedError

    def prepare(self, resume: str) -> None:
        """Remember which backend session the next process should continue ("" for a new one)."""
        self.resume = resume
        self.session = resume

    async def handshake(self) -> None:
        """Protocol setup after the process started."""

    async def job(self, prompt: str) -> str:
        """Send one prompt and return the final assistant message."""
        raise NotImplementedError

    async def interrupt_backend(self) -> None:
        """Ask the backend to stop the current turn; closing the process is the fallback."""
        await self.close()

    def job_prompt(self, brief: str) -> str:
        return brief

    # ----- lifecycle -----

    @property
    def alive(self) -> bool:
        return self.process is not None and self.process.returncode is None

    @property
    def busy(self) -> bool:
        return self._lock.locked()

    async def start(self) -> None:
        confine = (self.spec.workspace.path,) if self.spec.confined else None
        self.process = await self.transport.spawn(self.command(), self.spec.workspace.path, confine=confine)
        self.lines = asyncio.Queue()
        self.stderr = bytearray()
        self.reader = asyncio.create_task(self._read_lines())
        self.stderr_task = asyncio.create_task(self._read_stderr())

    async def _read_lines(self) -> None:
        # Bind this process's stream and queue now: a later start() replaces both.
        stdout, lines = self.process.stdout, self.lines
        try:
            while True:
                try:
                    line = await stdout.readline()
                except ValueError:
                    logger.error("worker %s emitted a line over %d bytes; stopping it", self.spec.worker_id, OUTPUT_LIMIT)
                    break
                if not line:
                    break
                lines.put_nowait(line.decode("utf-8", "replace"))
        finally:
            lines.put_nowait(None)

    async def _read_stderr(self) -> None:
        stderr, buffer = self.process.stderr, self.stderr
        while chunk := await stderr.read(4096):
            buffer.extend(chunk)
            del buffer[:-STDERR_LIMIT]

    async def close(self) -> None:
        """Stop the process and its readers; safe to repeat.

        Everything tied to the current process is detached before the first await, so a
        run that starts a new process meanwhile is left alone.
        """
        self._cancel_idle()
        process, self.process = self.process, None
        tasks, self.reader, self.stderr_task = (self.reader, self.stderr_task), None, None
        if process is not None and process.returncode is None:
            if process.stdin is not None:
                process.stdin.close()
            await terminate(process)
        for task in tasks:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
        if process is not None:
            release(process)

    def _cancel_idle(self) -> None:
        if self.idle_timer is not None:
            self.idle_timer.cancel()
            self.idle_timer = None

    def _schedule_idle(self) -> None:
        self._cancel_idle()
        loop = asyncio.get_running_loop()
        self.idle_timer = handle = loop.call_later(self.spec.idle_timeout, lambda: self._spawn_idle_close(handle))

    def _spawn_idle_close(self, handle: asyncio.TimerHandle) -> None:
        self._idle_task = asyncio.get_running_loop().create_task(self._idle_close(handle))

    async def _idle_close(self, handle: asyncio.TimerHandle) -> None:
        if self._lock.locked() or self.idle_timer is not handle:
            return
        async with self._lock:
            if self.idle_timer is handle:
                logger.info("worker %s idle; closing its process", self.spec.worker_id)
                await self.close()

    # ----- protocol helpers -----

    async def send(self, message: dict) -> None:
        if not self.alive or self.process.stdin is None:
            raise BackendError("worker process is not running")
        self.process.stdin.write((json.dumps(message) + "\n").encode())
        await self.process.stdin.drain()

    async def receive(self) -> dict:
        """The next JSON object from the process; raises when it ended."""
        while True:
            line = await self.lines.get()
            if line is None:
                status = self.process.returncode if self.process is not None else None
                detail = diagnostic(bytes(self.stderr))
                if any(marker in detail for marker in RESUME_FAILURES):
                    raise SessionUnavailable(detail)
                raise BackendError(f"worker process exited (status {status}): {self.transport.failure(status or 0, detail)}")
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if isinstance(message, dict):
                return message

    async def approve(self, request: ApprovalRequest) -> str:
        """Ask the job's approval handler, remembering allow-for-session decisions.

        An interrupt while the owner has not decided yet denies the request, so the
        backend can wind the turn down instead of waiting for the approval timeout.
        """
        if request.cache_key and request.cache_key in self._approved:
            return ALLOW_SESSION
        if self._interrupted.is_set():
            return DENY
        handler = asyncio.ensure_future(self._on_approval(request))
        interrupted = asyncio.ensure_future(self._interrupted.wait())
        try:
            await asyncio.wait({handler, interrupted}, return_when=asyncio.FIRST_COMPLETED)
        finally:
            interrupted.cancel()
        if not handler.done():
            handler.cancel()
            await asyncio.gather(handler, return_exceptions=True)
            return DENY
        try:
            decision = handler.result()
        except Exception:
            logger.exception("approval handler failed for worker %s; denying", self.spec.worker_id)
            return DENY
        if decision == ALLOW_SESSION and request.cache_key:
            self._approved.add(request.cache_key)
        return decision

    async def interrupt(self) -> None:
        """Stop the current job: deny any pending approval and ask the backend to end the turn."""
        if not self._lock.locked():
            return
        self._interrupted.set()
        await self.interrupt_backend()

    # ----- the one operation callers use -----

    async def run(self, brief: str, *, resume: str = "", on_approval: ApprovalHandler = deny_all) -> Outcome:
        """Run one job and return its WorkerResult and the backend session to resume next time.

        A stale session is retried once in a fresh process with a new session, so a lost
        identifier can never wedge a thread. When the reply lacks a WorkerResult, one
        follow-up turn asks for it; failing that, the prose becomes a partial result.
        """
        async with self._lock:
            self._cancel_idle()
            self._interrupted.clear()
            self._on_approval = on_approval
            try:
                prompt = self.job_prompt(brief)
                try:
                    text = await self._attempt(prompt, resume)
                except SessionUnavailable:
                    if not resume:
                        raise
                    logger.info("worker %s could not resume %s; starting a new session", self.spec.worker_id, resume)
                    await self.close()
                    text = await self._attempt(prompt, "")
                result = parse(text)
                if result is None and not self._interrupted.is_set():
                    try:
                        result = parse(await self._attempt(SUMMARIZE_PROMPT, self.session))
                    except BackendError as error:
                        logger.warning("worker %s could not summarize: %s", self.spec.worker_id, error)
                    result = result or fallback(text)
            except BaseException:
                await self.close()
                raise
            finally:
                self._on_approval = deny_all
            self._schedule_idle()
            return Outcome(result, self.session)

    async def _attempt(self, prompt: str, resume: str) -> str:
        if not self.alive or (resume and self.session != resume):
            await self.close()
            self.prepare(resume)
            await self.start()
            await self.handshake()
        try:
            return await asyncio.wait_for(self.job(prompt), self.spec.job_timeout)
        except TimeoutError:
            raise BackendError(f"job exceeded {self.spec.job_timeout:g}s") from None
