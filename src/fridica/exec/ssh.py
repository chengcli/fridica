"""Processes on an SSH host: ``ssh -T host 'exec sh -c …'``.

The agent CLI runs on the remote machine, next to its files, GPUs, and toolchain;
stdin/stdout carry the JSONL protocol through SSH unchanged. Every process for one
host shares a multiplexed connection (``ControlMaster``), so a burst of workers is
one TCP connection and one key exchange, not a connection storm.
"""

from __future__ import annotations

import os
from pathlib import Path, PurePath, PurePosixPath
import shlex
import stat

from . import process
from .sandbox import confinement, prepare_script, shell_words
from .transport import ARTIFACT_LIMIT, Launch, Transport

SSH_OPTIONS = ["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30",
               "-o", "ServerAliveCountMax=4"]
CONTROL_PERSIST = 600
SSH_FAILURE = 255
WORKSPACE_MISSING = 98
READ_TIMEOUT = 60


def control_directory() -> Path:
    """An owner-only directory with a short path for control sockets (Unix socket paths max ~100 bytes)."""
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    directory = Path(runtime) / "fridica" if runtime and Path(runtime).is_dir() else Path(f"/tmp/fridica-ssh-{os.getuid()}")
    directory.mkdir(mode=0o700, exist_ok=True)
    status = os.lstat(directory)
    if not stat.S_ISDIR(status.st_mode) or status.st_uid != os.getuid() or status.st_mode & 0o077:
        raise OSError(f"{directory} must be a directory owned by this user with mode 0700")
    return directory


def ssh_command(host: str, script: str) -> list[str]:
    control = ["-o", "ControlMaster=auto", "-o", f"ControlPath={control_directory() / '%C'}",
               "-o", f"ControlPersist={CONTROL_PERSIST}"]
    return ["ssh", *SSH_OPTIONS, *control, "--", host, script]


def cd(path: PurePath) -> str:
    text = str(path)
    if text.startswith("~/"):
        return f'cd "$HOME"/{shlex.quote(text[2:])}'
    return f"cd {shlex.quote(text)}"


def quoted_path(path: PurePath) -> str:
    text = str(path)
    return f'"$HOME"/{shlex.quote(text[2:])}' if text.startswith("~/") else shlex.quote(text)


def remote_script(command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
                  timeout: float | None = None, prefix: str = "", prepare: str = "") -> str:
    """One ``sh -c`` line: export env, cd (exit 98 if missing), and exec the command, bounded by ``timeout``.

    Handing a single quoted string to ``sh -c`` means the remote login shell only has
    to pass it through; bash, zsh, dash, and ksh do. csh-family login shells are not
    supported (they history-expand ``!`` inside quotes).
    """
    lines = ["set -u"]
    for name, value in (env or {}).items():
        lines.append(f"export {name}={shlex.quote(value)}")
    if prepare:
        lines.append(prepare)
    lines.append(f"{cd(cwd)} || exit {WORKSPACE_MISSING}")
    agent = (prefix + " " if prefix else "") + shlex.join(command)
    if timeout is not None:
        seconds = max(1, int(timeout) + 5)
        lines.append(f"if command -v timeout >/dev/null 2>&1; then exec timeout -k 5 {seconds} {agent}; else exec {agent}; fi")
    else:
        lines.append(f"exec {agent}")
    return "exec sh -c " + shlex.quote("; ".join(lines))


class SshTransport(Transport):
    def launch(self, command: list[str], cwd: PurePath, *, env: dict[str, str] | None = None,
               timeout: float | None = None, confine: tuple[PurePath, ...] | None = None) -> Launch:
        prefix = shell_words(confinement(confine, home=None)) if confine is not None else ""
        script = remote_script(command, cwd, env={**self.machine.resources.environment(), **(env or {})},
                               timeout=timeout, prefix=prefix, prepare=prepare_script() if confine is not None else "")
        return Launch(ssh_command(self.machine.host, script), None, process.scrubbed_environment(self.excluded_env))

    def failure(self, returncode: int, detail: str) -> str:
        host = self.machine.host
        if returncode == SSH_FAILURE:
            return (f"could not reach {host} over SSH; check that `ssh {host}` connects without a prompt"
                    + (f" ({detail})" if detail else ""))
        if returncode == WORKSPACE_MISSING:
            return f"the workspace directory does not exist on {host}"
        return super().failure(returncode, detail)

    async def read_file(self, path: PurePath, *, roots: tuple[PurePath, ...], limit: int = ARTIFACT_LIMIT) -> bytes:
        """One round trip: the file's real path, each root's real path, a NUL, then the bytes."""
        root_words = " ".join(quoted_path(root) for root in roots)
        script = (f"f=$(realpath -e -- {quoted_path(path)}) || exit 3; test -f \"$f\" || exit 4; printf '%s\\0' \"$f\"; "
                  f"for r in {root_words}; do printf '%s\\0' \"$(realpath -e -- \"$r\" 2>/dev/null || echo -)\"; done; "
                  f"printf '\\0'; head -c {limit + 1} -- \"$f\"")
        argv = ssh_command(self.machine.host, "exec sh -c " + shlex.quote(script))
        completed = await process.run_once(argv, cwd=None, env=process.scrubbed_environment(self.excluded_env),
                                           timeout=READ_TIMEOUT, limit=limit + 64 * 1024)
        header, separator, data = completed.stdout.partition(b"\0\0")
        if completed.returncode != 0 or not separator:
            raise ValueError(f"{path} could not be read on {self.machine.name}")
        real, *resolved = header.decode("utf-8", "replace").split("\0")
        allowed = [PurePosixPath(item) for item in resolved if item.startswith("/")]
        if not any(PurePosixPath(real) == root or PurePosixPath(real).is_relative_to(root) for root in allowed):
            raise ValueError(f"{path} is outside the workspace")
        if len(data) > limit:
            raise ValueError(f"{path} is larger than {limit} bytes")
        return data

    async def probe(self, command: list[str], *, timeout: float = 30) -> process.Completed:
        """Run a short command in the login shell (used by doctor)."""
        argv = ssh_command(self.machine.host, "exec sh -c " + shlex.quote(shlex.join(command)))
        return await process.run_once(argv, cwd=None, env=process.scrubbed_environment(self.excluded_env),
                                      timeout=timeout)
