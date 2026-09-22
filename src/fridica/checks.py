"""Local environment checks shared by ``fridica doctor`` and ``fridica start``.

Each check returns a list of human-readable problems, empty when the check passes.
They inspect the installed CLI, its sandbox dependencies, and its sign-in state
without ever invoking a model.
"""
from __future__ import annotations

import json
import shutil
import subprocess
import sys

from .config import Config
from .runner import diagnostic, environment

SANDBOX_TOOLS = {"linux": ("bwrap", "socat")}


def check_backend(config: Config) -> list[str]:
    executable = shutil.which(config.backend)
    if executable is None:
        return [f"Install {config.backend} and authenticate locally before starting Fridica."]
    command = [executable, "exec", "--help"] if config.backend == "codex" else [executable, "--help"]
    required = (["--ignore-user-config", "--ignore-rules", "--output-schema", "--ephemeral", "resume"]
                if config.backend == "codex" else
                ["--setting-sources", "--strict-mcp-config", "--json-schema", "dontAsk", "acceptEdits",
                 "--session-id", "--resume"])
    if config.file_access and config.backend == "codex":
        required += ["--strict-config"]
    try:
        result = subprocess.run(command, capture_output=True, text=True, timeout=10, env=environment(config))
    except (OSError, subprocess.TimeoutExpired):
        return [f"Could not inspect {config.backend}; check its installation."]
    if result.returncode or any(flag not in result.stdout for flag in required):
        return [f"Upgrade {config.backend}: required isolation/structured-output flags are unavailable."]
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
    if sys.platform != "linux":
        return []
    bwrap = shutil.which("bwrap")
    if config.backend == "claude":
        missing = [tool for tool in SANDBOX_TOOLS["linux"] if shutil.which(tool) is None]
        if missing:
            names = ", ".join(missing)
            return [f"Install {names} (for example: sudo apt install bubblewrap socat); "
                    f"the Claude sandbox cannot start without them. {SANDBOX_HELP}."]
    elif bwrap is None:
        return []  # Codex falls back to its bundled bwrap, which cannot be probed from here.
    try:
        result = subprocess.run(
            [bwrap, *SANDBOX_PROBE], capture_output=True, text=True,
            stdin=subprocess.DEVNULL, timeout=10, env=environment(config),
        )
    except subprocess.TimeoutExpired:
        return [f"The bubblewrap sandbox probe timed out; {SANDBOX_HELP}."]
    except OSError:
        return [f"Could not run bwrap; repair the bubblewrap installation. {SANDBOX_HELP}."]
    if result.returncode:
        detail = diagnostic(result.stderr.encode())
        hint = ("On Ubuntu 24.04 and later, check sysctl kernel.apparmor_restrict_unprivileged_userns "
                "and add the AppArmor profile for bwrap")
        return [f"The sandbox cannot create user namespaces ({detail or 'bwrap exited with status ' + str(result.returncode)}). "
                f"{hint}; {SANDBOX_HELP}."]
    return []


def check_authentication(config: Config) -> list[str]:
    executable = shutil.which(config.backend)
    if executable is None:
        return [f"Install {config.backend} before checking sign-in."]
    arguments = ["auth", "status"] if config.backend == "claude" else ["login", "status"]
    try:
        result = subprocess.run(
            [executable, *arguments], capture_output=True, text=True,
            stdin=subprocess.DEVNULL, timeout=10, env=environment(config),
        )
    except subprocess.TimeoutExpired:
        return [f"{config.backend} sign-in check timed out; run {' '.join([config.backend, *arguments])} locally."]
    except OSError:
        return [f"Could not run {config.backend}; repair its installation and retry."]
    if result.returncode:
        return [f"{config.backend} is not signed in or its status command failed; run {' '.join([config.backend, *arguments])} locally."]
    if config.backend == "claude":
        try:
            status = json.loads(result.stdout)
        except (ValueError, TypeError):
            return ["Claude returned an unreadable authentication status; run claude auth status locally."]
        if not isinstance(status, dict) or status.get("loggedIn") is not True:
            return ["Claude is not signed in; run claude to sign in, then retry."]
    return []
