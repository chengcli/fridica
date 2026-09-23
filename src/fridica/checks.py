"""Environment checks shared by ``fridica doctor`` and ``fridica start``.

Each check returns a list of human-readable problems, empty when the check passes.
They inspect the installed CLI, its sandbox dependencies, and its sign-in state
without ever invoking a model. With a remote working folder every probe runs on
the SSH host, because that is where the agent will run.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys

from . import remote
from .config import Config
from .runner import diagnostic, environment

SANDBOX_TOOLS = {"linux": ("bwrap", "socat")}
PLATFORMS = {"Linux": "linux", "Darwin": "darwin"}


def where(config: Config) -> str:
    """``locally`` or ``on <host>``, for messages that tell the owner where to fix something."""
    return f"on {config.ssh_host}" if config.remote else "locally"


def probe(config: Config, command: list[str], *, timeout: float = 10,
          stdin=subprocess.DEVNULL) -> subprocess.CompletedProcess:
    """Run a short diagnostic command where the agent runs and capture its output.

    Raises ``OSError`` or ``subprocess.TimeoutExpired`` like ``subprocess.run``; a
    remote connection failure surfaces as exit status 255 with ssh's stderr.
    """
    if config.remote:
        command = remote.ssh_command(config, remote.remote_script(command, config.workspace, timeout=timeout))
    return subprocess.run(command, capture_output=True, text=True, stdin=stdin, timeout=timeout, env=environment(config))


def which(config: Config, name: str) -> str | None:
    """The executable's path where the agent runs, or None when it is not on PATH there."""
    if not config.remote:
        return shutil.which(name)
    try:
        result = probe(config, ["command", "-v", name])
    except (OSError, subprocess.TimeoutExpired):
        return None
    found = result.stdout.strip().splitlines()
    return found[-1] if result.returncode == 0 and found else None


def platform(config: Config) -> str | None:
    """``sys.platform``-style name of the host that runs the agent, or None when unknown."""
    if not config.remote:
        return sys.platform
    try:
        result = probe(config, ["uname", "-s"])
    except (OSError, subprocess.TimeoutExpired):
        return None
    return PLATFORMS.get(result.stdout.strip()) if result.returncode == 0 else None


def check_connection(config: Config) -> list[str]:
    """Report why the SSH host that owns the working folder cannot be used; empty for local configurations."""
    if not config.remote:
        return []
    host = config.ssh_host
    try:
        result = probe(config, ["uname", "-s"], timeout=30)
    except subprocess.TimeoutExpired:
        return [f"Connecting to {host} timed out; check ssh {host} from this machine."]
    except OSError:
        return ["Could not run ssh; install an OpenSSH client."]
    if result.returncode == remote.SSH_FAILURE:
        detail = diagnostic(result.stderr.encode())
        return [f"Could not connect to {host} without a prompt ({detail or 'ssh exited with status 255'}); "
                f"set up key authentication and a Host entry in ~/.ssh/config."]
    if result.returncode == 98:
        return [f"The workspace {config.root_label(config.workspace)} is not a directory on {host}."]
    if result.returncode:
        detail = diagnostic(result.stderr.encode())
        return [f"A command on {host} failed with status {result.returncode}"
                + (f" ({detail})" if detail else "") + "; check the login shell there."]
    if result.stdout.strip() not in PLATFORMS:
        return [f"{host} runs {result.stdout.strip() or 'an unknown system'}; Linux or macOS is required."]
    problems = []
    for directory in (*config.additional_workspaces, *config.read_only_workspaces):
        try:
            result = probe(config, ["test", "-d", str(directory)])
        except (OSError, subprocess.TimeoutExpired):
            return [f"Could not inspect {config.root_label(directory)}; check ssh {host}."]
        if result.returncode:
            problems.append(f"{config.root_label(directory)} is not a directory on {host}.")
    return problems


def unreachable(config: Config, result: subprocess.CompletedProcess) -> list[str]:
    """The problem to report when a remote probe never reached the agent's host; empty otherwise."""
    if config.remote and result.returncode == remote.SSH_FAILURE:
        detail = diagnostic(result.stderr.encode())
        return [f"Could not reach {config.ssh_host} over SSH ({detail or 'ssh exited with status 255'})."]
    return []


def check_backend(config: Config) -> list[str]:
    executable = which(config, config.backend)
    if executable is None:
        return [f"Install {config.backend} and authenticate {where(config)} before starting Fridica."]
    command = [executable, "exec", "--help"] if config.backend == "codex" else [executable, "--help"]
    required = (["--ignore-user-config", "--ignore-rules", "--output-schema", "--ephemeral", "resume"]
                if config.backend == "codex" else
                ["--setting-sources", "--strict-mcp-config", "--json-schema", "dontAsk", "acceptEdits",
                 "--session-id", "--resume"])
    if config.file_access and config.backend == "codex":
        required += ["--strict-config"]
    try:
        result = probe(config, command, stdin=None)
    except (OSError, subprocess.TimeoutExpired):
        return [f"Could not inspect {config.backend} {where(config)}; check its installation."]
    if unreachable(config, result):
        return unreachable(config, result)
    if result.returncode or any(flag not in result.stdout for flag in required):
        return [f"Upgrade {config.backend} {where(config)}: required isolation/structured-output flags are unavailable."]
    if config.heavy_tasks:
        if config.backend == "codex":
            try:
                result = probe(config, [executable, "app-server", "--help"], stdin=None)
            except (OSError, subprocess.TimeoutExpired):
                return [f"Could not inspect codex app-server {where(config)}; check its installation."]
            if result.returncode or "--listen" not in result.stdout:
                return [f"Upgrade codex {where(config)}: heavy_tasks needs the codex app-server command."]
        elif "--input-format" not in result.stdout:
            return [f"Upgrade claude {where(config)}: heavy_tasks needs --input-format stream-json."]
    return []


SANDBOX_PROBE = ["--unshare-user", "--unshare-net", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                 "--die-with-parent", "--", "/bin/true"]
SANDBOX_HELP = "see README: Sandbox dependencies"


def check_sandbox(config: Config) -> list[str]:
    """Report why the backend's mandatory sandbox could not run on this host.

    Both backends sandbox commands with bubblewrap on Linux. Claude requires the
    system ``bwrap`` and ``socat`` packages and is started with
    ``sandbox.failIfUnavailable``, so a missing dependency makes every agent
    invocation exit immediately. Codex bundles its own ``bwrap`` and needs no
    ``socat``, but prefers a system ``bwrap`` found on ``PATH``. Either way, bwrap
    must be allowed to create user namespaces; Ubuntu 24.04 and later restrict
    this through AppArmor by default, which surfaces as a ``bwrap:`` error on
    every sandboxed command. The probe runs a trivial command inside a bwrap
    namespace so that ``doctor`` and ``start`` report the problem before Slack
    users see failed replies.
    """
    if platform(config) != "linux":
        return []
    bwrap = which(config, "bwrap")
    if config.backend == "claude":
        missing = [tool for tool in SANDBOX_TOOLS["linux"] if which(config, tool) is None]
        if missing:
            names = ", ".join(missing)
            return [f"Install {names} {where(config)} (for example: sudo apt install bubblewrap socat); "
                    f"the Claude sandbox cannot start without them. {SANDBOX_HELP}."]
    elif bwrap is None:
        return []  # Codex falls back to its bundled bwrap, which cannot be probed from here.
    try:
        result = probe(config, [bwrap, *SANDBOX_PROBE])
    except subprocess.TimeoutExpired:
        return [f"The bubblewrap sandbox probe timed out; {SANDBOX_HELP}."]
    except OSError:
        return [f"Could not run bwrap; repair the bubblewrap installation. {SANDBOX_HELP}."]
    if unreachable(config, result):
        return unreachable(config, result)
    if result.returncode:
        detail = diagnostic(result.stderr.encode())
        hint = ("On Ubuntu 24.04 and later, check sysctl kernel.apparmor_restrict_unprivileged_userns "
                "and add the AppArmor profile for bwrap")
        return [f"The sandbox cannot create user namespaces ({detail or 'bwrap exited with status ' + str(result.returncode)}). "
                f"{hint}; {SANDBOX_HELP}."]
    return []


def check_authentication(config: Config) -> list[str]:
    executable = which(config, config.backend)
    if executable is None:
        return [f"Install {config.backend} {where(config)} before checking sign-in."]
    arguments = ["auth", "status"] if config.backend == "claude" else ["login", "status"]
    try:
        result = probe(config, [executable, *arguments])
    except subprocess.TimeoutExpired:
        return [f"{config.backend} sign-in check timed out; run {' '.join([config.backend, *arguments])} {where(config)}."]
    except OSError:
        return [f"Could not run {config.backend} {where(config)}; repair its installation and retry."]
    if unreachable(config, result):
        return unreachable(config, result)
    if result.returncode:
        return [f"{config.backend} is not signed in or its status command failed; run {' '.join([config.backend, *arguments])} {where(config)}."]
    if config.backend == "claude":
        try:
            status = json.loads(result.stdout)
        except (ValueError, TypeError):
            return [f"Claude returned an unreadable authentication status; run claude auth status {where(config)}."]
        if not isinstance(status, dict) or status.get("loggedIn") is not True:
            return [f"Claude is not signed in; run claude {where(config)} to sign in, then retry."]
    return []
