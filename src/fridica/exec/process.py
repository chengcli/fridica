"""Subprocess plumbing: scrubbed environment, bounded output, and process-group termination."""

from __future__ import annotations

import asyncio
from collections.abc import Iterable
from dataclasses import dataclass
import os
from pathlib import Path
import signal

from ..core.errors import BackendError

OUTPUT_LIMIT = 4 * 1024 * 1024
DIAGNOSTIC_LIMIT = 500
TOKEN_PREFIXES = ("xoxp-", "xoxb-", "xapp-", "xoxe-")


def scrubbed_environment(excluded: Iterable[str] = (), extra: dict[str, str] | None = None) -> dict[str, str]:
    """The daemon's environment without Slack tokens or anything that looks like one.

    Agents run arbitrary commands, so they must never see the credentials that let
    Fridica post as the owner.
    """
    excluded = set(excluded)
    environment = {key: value for key, value in os.environ.items()
                   if key not in excluded and "SLACK" not in key.upper() and not value.startswith(TOKEN_PREFIXES)}
    environment.update(extra or {})
    return environment


def diagnostic(stderr: bytes | str, limit: int = DIAGNOSTIC_LIMIT) -> str:
    """A bounded single-line tail of stderr, for the local log only."""
    text = stderr.decode("utf-8", "replace") if isinstance(stderr, bytes) else stderr
    return " ".join(text.split())[-limit:]


async def terminate(process: asyncio.subprocess.Process, grace: float = 1.0) -> None:
    """SIGTERM the whole process group, then SIGKILL after ``grace`` seconds."""
    if process.returncode is not None:
        return
    for sig, wait in ((signal.SIGTERM, grace), (signal.SIGKILL, None)):
        try:
            os.killpg(process.pid, sig)
        except ProcessLookupError:
            break
        except PermissionError:
            # macOS reports EPERM, not ESRCH, for a group whose members exited but were not reaped yet.
            # Signal the child directly as a fallback; if it is gone too, just reap it.
            try:
                process.send_signal(sig)
            except (ProcessLookupError, PermissionError):
                break
        try:
            await asyncio.wait_for(process.wait(), wait)
            break
        except TimeoutError:
            continue
    await process.wait()


@dataclass(frozen=True)
class Completed:
    returncode: int
    stdout: bytes
    stderr: bytes

    @property
    def text(self) -> str:
        return self.stdout.decode("utf-8", "replace")


async def _read_limited(stream: asyncio.StreamReader, limit: int) -> bytes:
    output = bytearray()
    while chunk := await stream.read(65536):
        output.extend(chunk)
        if len(output) > limit:
            raise BackendError("process output exceeded the size limit")
    return bytes(output)


async def start(argv: list[str], *, cwd: Path | None, env: dict[str, str]) -> asyncio.subprocess.Process:
    """Start a long-lived process with piped stdio in its own process group."""
    try:
        return await asyncio.create_subprocess_exec(
            *argv, cwd=cwd, env=env, stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE, start_new_session=True, limit=OUTPUT_LIMIT)
    except OSError as error:
        raise BackendError(f"could not start {argv[0]}: {error}") from error


async def run_once(argv: list[str], *, stdin: bytes = b"", cwd: Path | None, env: dict[str, str], timeout: float,
                   limit: int = OUTPUT_LIMIT) -> Completed:
    """Run a command to completion with bounded output; kill its process group on timeout or cancellation."""
    process = await start(argv, cwd=cwd, env=env)

    async def communicate() -> Completed:
        readers = [asyncio.create_task(_read_limited(process.stdout, limit)),
                   asyncio.create_task(_read_limited(process.stderr, limit))]
        try:
            if stdin:
                process.stdin.write(stdin)
                await process.stdin.drain()
            process.stdin.close()
            stdout, stderr = await asyncio.gather(*readers)
            await process.wait()
            return Completed(process.returncode, stdout, stderr)
        finally:
            for reader in readers:
                reader.cancel()
            await asyncio.gather(*readers, return_exceptions=True)

    try:
        return await asyncio.wait_for(communicate(), timeout)
    except TimeoutError:
        await terminate(process)
        raise BackendError(f"{argv[0]} timed out after {timeout:g}s") from None
    except BaseException:
        await terminate(process)
        raise
