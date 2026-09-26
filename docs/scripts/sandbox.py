"""Measure what a sandboxed agent can see and do, by running sandbox_probe.sh in three settings.

* host: no sandbox (the baseline);
* fridica: Fridica's own bubblewrap confinement (``gpu_confine``), with the argv built by ``fridica.exec.sandbox``;
* claude: Claude Code's sandbox with the settings Fridica gives its Claude workers. This needs one model call, so it
  runs only on request (``--probe-claude``); otherwise the last measurement is reused from the committed JSON file.

Nothing outside a temporary directory is modified: the probe only writes inside its own workspace and /tmp.
"""

from __future__ import annotations

import datetime as dt
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

from common import GENERATED

HERE = Path(__file__).resolve().parent
PROBE = HERE / "sandbox_probe.sh"
RESULTS = GENERATED / "sandbox_probe.json"
NAMESPACES = ("user", "mnt", "pid", "net", "ipc", "uts")

# The settings document Fridica passes to Claude workers in write mode (workers/claude.py), with no network domains.
CLAUDE_SETTINGS = {"disableAllHooks": True, "disableClaudeAiConnectors": True, "enabledPlugins": {},
                   "autoMemoryEnabled": False,
                   "sandbox": {"enabled": True, "failIfUnavailable": True, "autoAllowBashIfSandboxed": True,
                               "allowUnsandboxedCommands": False, "excludedCommands": [],
                               "network": {"allowedDomains": [], "allowLocalBinding": False}}}
CLAUDE_PROMPT = ("Run exactly this one Bash command and then print its full output verbatim, nothing else: "
                 "sh probe.sh")


def workspace(directory: Path) -> tuple[Path, subprocess.Popen, Path]:
    """A workspace holding the probe and ref.env (host namespace ids, a host PID, a host /tmp marker)."""
    directory.mkdir(parents=True)
    shutil.copy(PROBE, directory / "probe.sh")
    sleeper = subprocess.Popen(["sleep", "600"])
    handle, marker = tempfile.mkstemp(prefix="fridica-probe-marker.")
    os.close(handle)
    lines = [f"HOST_NS_{name}='{os.readlink(f'/proc/self/ns/{name}')}'" for name in NAMESPACES]
    lines += [f"HOST_PID={sleeper.pid}", f"HOST_TMP_MARKER={marker}"]
    (directory / "ref.env").write_text("\n".join(lines) + "\n")
    return directory, sleeper, Path(marker)


def parse(output: str) -> dict[str, str]:
    values = {}
    for line in output.splitlines():
        key, sep, value = line.strip().strip("`").partition("=")
        if sep and key.replace("_", "").isalnum() and " " not in key:
            values[key] = value.strip()
    return values


def run(kind: str) -> dict[str, str]:
    with tempfile.TemporaryDirectory(prefix="fridica-sandbox-") as root:
        directory, sleeper, marker = workspace(Path(root) / "ws")
        try:
            if kind == "host":
                command, stdin = ["sh", str(directory / "probe.sh")], None
            elif kind == "fridica":
                from fridica.exec.sandbox import confinement
                command = confinement([directory], home=str(Path.home())) + ["sh", str(directory / "probe.sh")]
                stdin = None
            else:
                command = ["claude", "-p", "--model", "haiku", "--setting-sources", "", "--settings",
                           json.dumps(CLAUDE_SETTINGS), "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
                           "--permission-mode", "acceptEdits", "--tools", "Bash,Read", "--allowedTools", "Read"]
                stdin = CLAUDE_PROMPT
            result = subprocess.run(command, cwd=directory, input=stdin, capture_output=True, text=True, timeout=300)
            return parse(result.stdout)
        finally:
            sleeper.kill()
            sleeper.wait()
            marker.unlink(missing_ok=True)


def version(command: list[str]) -> str:
    try:
        return subprocess.run(command, capture_output=True, text=True, timeout=30).stdout.strip().splitlines()[0]
    except (OSError, IndexError, subprocess.TimeoutExpired):
        return "unavailable"


def collect(probe_claude: bool = False) -> dict:
    """Fresh host and Fridica measurements (when bwrap exists); the Claude measurement fresh only on request."""
    cached = json.loads(RESULTS.read_text()) if RESULTS.exists() else {}
    data = dict(cached)
    data["host"] = run("host")
    if shutil.which("bwrap"):
        data["fridica"] = run("fridica")
        data["bwrap"] = version(["bwrap", "--version"])
    if probe_claude:
        data["claude"] = run("claude")
        data["claude_version"] = version(["claude", "--version"])
        data["claude_measured"] = dt.date.today().isoformat()
    data["kernel"] = os.uname().release
    data["measured"] = dt.date.today().isoformat()
    try:
        data["userns_restricted"] = Path("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").read_text().strip()
    except OSError:
        data["userns_restricted"] = "n/a"
    RESULTS.parent.mkdir(parents=True, exist_ok=True)
    RESULTS.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")
    return data
