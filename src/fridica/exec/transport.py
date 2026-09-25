"""The Transport protocol: how a machine's processes are started.

A worker is an agent CLI speaking JSONL on stdio. The transport only decides where
that process runs: here (``local``), on an SSH host (``ssh -T``, no PTY, so stdio
carries the protocol unchanged), or on a Slurm allocation reached through a login
host (``slurm``, not implemented yet). Nothing listens on a port; SSH is the
security boundary.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from pathlib import Path, PurePath

from ..machines.registry import Machine
from . import process
from .process import Completed

ARTIFACT_LIMIT = 20 * 1024 * 1024


@dataclass(frozen=True)
class Launch:
    """A ready-to-exec local argv, the local cwd (None when the transport changes directory remotely), and env."""
    argv: list[str]
    cwd: Path | None
    env: dict[str, str] = field(default_factory=dict)


class Transport(ABC):
    def __init__(self, machine: Machine, *, excluded_env: tuple[str, ...] = ()):
        self.machine = machine
        self.excluded_env = excluded_env

    @abstractmethod
    def launch(self, command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
               timeout: float | None = None, confine: tuple[PurePath, ...] | None = None) -> Launch:
        """Wrap ``command`` so it runs in ``cwd`` on this machine.

        ``confine`` lists the only directories the process may write, under Fridica's
        bubblewrap confinement (see ``sandbox``); None runs it unconfined.
        """

    @abstractmethod
    async def read_file(self, path: PurePath, *, roots: tuple[PurePath, ...], limit: int = ARTIFACT_LIMIT) -> bytes:
        """Read a file whose real path lies inside ``roots``; ValueError otherwise."""

    def failure(self, returncode: int, detail: str) -> str:
        return (f"agent on {self.machine.name} exited with status {returncode}; check its sign-in and sandbox support"
                + (f" ({detail})" if detail else ""))

    async def run(self, command: list[str], cwd: PurePath, *, stdin: bytes = b"", timeout: float,
                  env: dict[str, str] | None = None) -> Completed:
        spec = self.launch(command, cwd, env=env, timeout=timeout)
        return await process.run_once(spec.argv, stdin=stdin, cwd=spec.cwd, env=spec.env, timeout=timeout + 10)

    async def spawn(self, command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
                    confine: tuple[PurePath, ...] | None = None):
        spec = self.launch(command, cwd, env=env, confine=confine)
        return await process.start(spec.argv, cwd=spec.cwd, env=spec.env)


def make_transport(machine: Machine, *, excluded_env: tuple[str, ...] = ()) -> Transport:
    from .local import LocalTransport
    from .slurm import SlurmTransport
    from .ssh import SshTransport
    kinds = {"local": LocalTransport, "ssh": SshTransport, "slurm": SlurmTransport}
    return kinds[machine.transport](machine, excluded_env=excluded_env)
