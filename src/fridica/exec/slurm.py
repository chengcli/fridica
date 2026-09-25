"""Slurm machines: registered and validated, not runnable yet.

The intended shape reuses the SSH transport: through the login host, allocate and
run the agent on a compute node with its stdio attached, for example::

    ssh -T login 'cd ws && exec srun --account=A --partition=P --gres=G --time=T \\
                  --unbuffered --pty=no codex app-server'

Open questions before implementing: an allocation's lifetime versus the worker's
idle timeout, queue waits longer than a thread's patience, and reporting job state
while pending.
"""

from __future__ import annotations

from pathlib import PurePath

from .transport import ARTIFACT_LIMIT, Launch, Transport


class SlurmNotImplemented(NotImplementedError):
    pass


class SlurmTransport(Transport):
    def launch(self, command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
               timeout: float | None = None, confine: bool = False) -> Launch:
        raise SlurmNotImplemented(f"machine {self.machine.name} uses Slurm, which Fridica does not support yet")

    async def read_file(self, path: PurePath, *, roots: tuple[PurePath, ...], limit: int = ARTIFACT_LIMIT) -> bytes:
        raise SlurmNotImplemented(f"machine {self.machine.name} uses Slurm, which Fridica does not support yet")

    async def probe(self, command: list[str], *, timeout: float = 30):
        raise SlurmNotImplemented(f"machine {self.machine.name} uses Slurm, which Fridica does not support yet")
