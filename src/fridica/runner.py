"""Run an agent CLI as a subprocess with bounded output and a scrubbed environment.

This module knows nothing about Slack, prompts, or which backend is in use. It
starts a command, feeds it the prompt on stdin, collects stdout and stderr within
size and time limits, and turns failures into ``BackendError`` values whose text
is safe to log locally.
"""
from __future__ import annotations

import asyncio
import os
from pathlib import Path
import signal

from .config import Config

OUTPUT_LIMIT = 4 * 1024 * 1024
DIAGNOSTIC_LIMIT = 500
RESUME_FAILURES = ("No conversation found with session ID", "no rollout found for thread id")


class BackendError(RuntimeError):
    pass


class SessionUnavailable(BackendError):
    """The backend could not find the persisted session Fridica asked it to resume."""


def environment(config: Config) -> dict[str, str]:
    """The parent environment minus Slack tokens and anything that looks like one."""
    excluded = {
        getattr(config, "app_token_env", "SLACK_APP_TOKEN"),
        getattr(config, "user_token_env", "SLACK_USER_TOKEN"),
    }
    return {
        key: value for key, value in os.environ.items()
        if key not in excluded and "SLACK" not in key.upper()
        and not value.startswith(("xoxp-", "xoxb-", "xapp-"))
    }


def diagnostic(stderr: bytes) -> str:
    """Return a bounded, single-line tail of the agent's stderr for local logs."""
    text = " ".join(stderr.decode("utf-8", "replace").split())
    return text[-DIAGNOSTIC_LIMIT:]


async def _read_limited(stream: asyncio.StreamReader) -> bytes:
    output = bytearray()
    while chunk := await stream.read(65536):
        output.extend(chunk)
        if len(output) > OUTPUT_LIMIT:
            raise BackendError("Agent output exceeded the size limit.")
    return bytes(output)


async def _terminate(process: asyncio.subprocess.Process) -> None:
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        await asyncio.wait_for(process.wait(), 1)
    except TimeoutError:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    await process.wait()


async def run(command: list[str], prompt: str, cwd: Path, config: Config) -> str:
    """Run ``command`` with ``prompt`` on stdin and return its stdout.

    Raises ``SessionUnavailable`` when the CLI reports that a session to resume no
    longer exists, and ``BackendError`` for any other non-zero exit, with a bounded
    stderr tail in the message.
    """
    try:
        process = await asyncio.create_subprocess_exec(
            *command, cwd=cwd, env=environment(config),
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE, start_new_session=True,
        )
    except OSError as error:
        raise BackendError("Could not start the configured agent executable.") from error

    async def communicate() -> tuple[bytes, bytes]:
        assert process.stdin is not None
        assert process.stdout is not None and process.stderr is not None
        readers = [asyncio.create_task(_read_limited(process.stdout)),
                   asyncio.create_task(_read_limited(process.stderr))]
        try:
            process.stdin.write(prompt.encode())
            await process.stdin.drain()
            process.stdin.close()
            stdout, stderr = await asyncio.gather(*readers)
            await process.wait()
            return stdout, stderr
        finally:
            for reader in readers:
                reader.cancel()
            await asyncio.gather(*readers, return_exceptions=True)

    try:
        output, diagnostics = await asyncio.wait_for(communicate(), config.timeout)
    except BaseException:
        await _terminate(process)
        raise
    if process.returncode:
        detail = diagnostic(diagnostics)
        if any(marker in detail for marker in RESUME_FAILURES):
            raise SessionUnavailable(f"Agent could not resume its session: {detail}")
        raise BackendError(
            f"Agent exited with status {process.returncode}; check local authentication and sandbox support."
            + (f" Agent stderr: {detail}" if detail else "")
        )
    return output.decode("utf-8")
