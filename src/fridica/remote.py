"""Run agent commands on the SSH host that owns the working folder.

A working folder such as ``dart9:/mnt/project`` means every agent process (the
per-turn CLI runs, the environment checks, and the persistent heavy-task worker)
starts on ``dart9`` through ``ssh -T``: no PTY, so stdin carries the prompt or the
protocol stream and stdout carries the agent's structured output unchanged. SSH
itself is the byte transport and the security boundary; nothing listens on a port.
"""
from __future__ import annotations

import os
from pathlib import Path, PurePath, PurePosixPath
import shlex
import stat
import uuid

from .config import Config

SSH_OPTIONS = ["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", "-o", "ServerAliveInterval=30"]
CONTROL_PERSIST = 600  # seconds the shared connection outlives its last use
SSH_FAILURE = 255  # OpenSSH's exit status when the connection itself failed


def control_directory() -> Path:
    """An owner-only directory with a short path for the multiplexed connection's control sockets.

    Unix socket paths are limited to about 100 bytes, so the sockets cannot live under
    an arbitrary state directory. The user's runtime directory is used when the system
    provides one, otherwise a per-user directory under ``/tmp`` that must be a real
    directory owned by this user.
    """
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    directory = Path(runtime) / "fridica" if runtime and Path(runtime).is_dir() else Path(f"/tmp/fridica-ssh-{os.getuid()}")
    directory.mkdir(mode=0o700, exist_ok=True)
    status = os.lstat(directory)
    if not stat.S_ISDIR(status.st_mode) or status.st_uid != os.getuid() or status.st_mode & 0o077:
        raise OSError(f"{directory} must be a directory owned by this user with mode 0700")
    return directory


def ssh_command(config: Config, script: str) -> list[str]:
    """The local argv that runs ``script`` in the remote user's login shell.

    Every run, check, and worker shares one multiplexed connection (``ControlMaster``),
    so a burst of agent calls does not look like a connection storm to the server and
    each call skips the key exchange.
    """
    control = ["-o", "ControlMaster=auto", "-o", f"ControlPath={control_directory() / '%C'}",
               "-o", f"ControlPersist={CONTROL_PERSIST}"]
    return ["ssh", *SSH_OPTIONS, *control, "--", config.ssh_host, script]


def remote_directory() -> PurePosixPath:
    """A fresh per-run scratch directory path on the remote host."""
    return PurePosixPath("/tmp") / f"fridica-agent-{uuid.uuid4().hex}"


def remote_script(command: list[str], cwd: PurePath, *, files: dict[str, str] | None = None,
                  directory: PurePosixPath | None = None, env: dict[str, str] | None = None,
                  timeout: float | None = None) -> str:
    """A POSIX ``sh`` script that materialises ``files`` in ``directory``, runs ``command`` in ``cwd``, and cleans up.

    Every path, file body, and argument is shell-quoted. When the remote host has
    ``timeout`` the agent is bounded by ``timeout`` seconds so a dropped SSH connection
    cannot leave it running forever. The script exits with the agent's status. It is
    handed to ``sh -c`` as one line so the remote user's login shell (bash, zsh, csh)
    only has to pass a single-quoted string through.
    """
    lines = ["set -u"]
    for name, value in (env or {}).items():
        lines.append(f"export {name}={shlex.quote(value)}")
    if directory is not None:
        # The trap body is literal; it expands the variable when it fires, so the path
        # is never re-parsed by the shell no matter what characters it contains.
        lines.append(f"FRIDICA_SCRATCH={shlex.quote(str(directory))}")
        lines.append('mkdir -m 700 "$FRIDICA_SCRATCH" || exit 97')
        lines.append("trap 'rm -rf \"$FRIDICA_SCRATCH\"' EXIT HUP INT TERM")
        for name, body in (files or {}).items():
            target = shlex.quote(str(directory / name))
            lines.append(f"printf '%s' {shlex.quote(body)} > {target} || exit 97")
    lines.append(f"cd {shlex.quote(str(cwd))} || exit 98")
    agent = shlex.join(command)
    if timeout is not None:
        seconds = max(1, int(timeout) + 5)
        lines.append(f"if command -v timeout >/dev/null 2>&1; then timeout -k 5 {seconds} {agent}; else {agent}; fi")
    else:
        lines.append(agent)
    return "exec sh -c " + shlex.quote("; ".join(lines))


def launch(config: Config, command: list[str], cwd: PurePath, *, files: dict[str, str] | None = None,
           directory: PurePosixPath | None = None, env: dict[str, str] | None = None,
           timeout: float | None = None) -> tuple[list[str], Path | None]:
    """The argv to start locally, and the local cwd to start it in.

    Local configurations return ``command`` and ``cwd`` unchanged. Remote ones return
    the ``ssh`` argv wrapping ``remote_script`` and ``None`` for the local cwd.
    """
    if not config.remote:
        return list(command), Path(cwd)
    script = remote_script(command, cwd, files=files, directory=directory, env=env, timeout=timeout)
    return ssh_command(config, script), None
