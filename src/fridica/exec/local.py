"""Processes on this machine."""

from __future__ import annotations

from pathlib import Path, PurePath

from . import process
from .sandbox import confinement, prepare_local
from .transport import ARTIFACT_LIMIT, Launch, Transport


class LocalTransport(Transport):
    def launch(self, command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
               timeout: float | None = None, confine: tuple[PurePath, ...] | None = None) -> Launch:
        prefix = []
        if confine is not None:
            prepare_local(str(Path.home()))
            prefix = confinement(confine, home=str(Path.home()))
        environment = process.scrubbed_environment(self.excluded_env,
                                                   {**self.machine.resources.environment(), **(env or {})})
        return Launch([*prefix, *command], Path(cwd).expanduser(), environment)

    async def read_file(self, path: PurePath, *, roots: tuple[PurePath, ...], limit: int = ARTIFACT_LIMIT) -> bytes:
        try:
            real = Path(path).expanduser().resolve(strict=True)
        except OSError:
            raise ValueError(f"{path} does not exist") from None
        allowed = [Path(root).expanduser().resolve() for root in roots]
        if not any(real == root or real.is_relative_to(root) for root in allowed):
            raise ValueError(f"{path} is outside the workspace")
        if not real.is_file():
            raise ValueError(f"{path} is not a regular file")
        with open(real, "rb") as stream:
            data = stream.read(limit + 1)
        if len(data) > limit:
            raise ValueError(f"{path} is larger than {limit} bytes")
        return data

    async def probe(self, command: list[str], *, timeout: float = 30) -> process.Completed:
        """Run a short command here (used by doctor)."""
        return await process.run_once(command, cwd=Path.home(), env=process.scrubbed_environment(self.excluded_env),
                                      timeout=timeout)
