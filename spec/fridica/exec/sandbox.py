"""Fridica's own bubblewrap confinement, used when a backend's sandbox would hide the GPUs.

Both backends sandbox commands with bubblewrap on Linux, whose minimal ``/dev``
hides GPU device nodes. A machine with ``policy.gpu_confine`` therefore runs the
backend with its own sandbox off inside this confinement instead: the filesystem is
read-only except the worker's own workspace, a private ``/tmp``, and the
backends' state directories, whose settings files stay read-only so a job cannot
plant hooks that later run unconfined in the owner's own sessions. Missing settings
files are created (empty) first, because bubblewrap can only bind a file that exists.

Residual risk, by design: the network is shared with the host (the CLI must reach
its model API), and ``~/.claude.json`` stays writable because Claude keeps state
there, so a confined job could add user-level MCP servers that the owner's own
interactive sessions would load. Fridica's own workers ignore them
(``--strict-mcp-config``). Use gpu_confine only on machines and workspaces you trust.
"""

from __future__ import annotations

import os
import shlex
from collections.abc import Iterable
from pathlib import PurePath

BWRAP = "bwrap"
BACKEND_STATE = (".codex", ".claude", ".claude.json")
BACKEND_SETTINGS = (".codex/config.toml", ".claude/settings.json", ".claude/settings.local.json", ".claude/hooks")


def confinement(roots: Iterable[PurePath], *, home: str | None) -> list[str]:
    """The bubblewrap argv prefix; ``home`` is None on a remote machine, where ``$HOME`` expands there."""
    words = [BWRAP, "--die-with-parent", "--unshare-user", "--unshare-pid", "--ro-bind", "/", "/",
             "--dev-bind", "/dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]
    for root in roots:
        path = home_relative(root) if home is None else str(root)
        words += ["--bind", path, path]
    for name in BACKEND_STATE:
        path = f"$HOME/{name}" if home is None else os.path.join(home, name)
        words += ["--bind-try", path, path]
    for name in BACKEND_SETTINGS:
        path = f"$HOME/{name}" if home is None else os.path.join(home, name)
        words += ["--ro-bind-try", path, path]
    return words + ["--"]


SETTINGS_DEFAULTS = {".claude/settings.json": "{}", ".claude/settings.local.json": "{}", ".codex/config.toml": ""}


def prepare_local(home: str) -> None:
    """Create missing settings files and the hooks directory so they can be bound read-only."""
    for directory in (".claude/hooks", ".codex"):
        os.makedirs(os.path.join(home, directory), mode=0o700, exist_ok=True)
    for name, body in SETTINGS_DEFAULTS.items():
        path = os.path.join(home, name)
        if not os.path.lexists(path):
            with open(path, "x") as stream:
                stream.write(body)


def prepare_script() -> str:
    """The same preparation as a POSIX sh fragment for a remote machine."""
    lines = ['mkdir -p "$HOME/.claude/hooks" "$HOME/.codex"']
    for name, body in SETTINGS_DEFAULTS.items():
        lines.append(f'[ -e "$HOME/{name}" ] || printf %s {shlex.quote(body)} > "$HOME/{name}"')
    return "; ".join(lines)


def home_relative(path: PurePath | str) -> str:
    """``~/x`` becomes ``$HOME/x`` so the remote shell expands it inside double quotes."""
    text = str(path)
    return "$HOME/" + text[2:] if text.startswith("~/") else text


def shell_words(argv: list[str]) -> str:
    """Quote words for sh, leaving leading ``$HOME/`` references expandable."""
    return " ".join(f'"$HOME/"{shlex.quote(word[6:])}' if word.startswith("$HOME/") else shlex.quote(word)
                    for word in argv)
