"""Checks behind ``fridica doctor`` and ``fridica start``.

Every probe runs where the process it vouches for will run: the parent backend
locally, each worker backend on its machine (through SSH for remote machines). No
model is ever called.
"""

from __future__ import annotations

import asyncio
from dataclasses import dataclass
import json
import os
from pathlib import Path
import shlex
import sys

from ..config.loader import load_config
from ..config.schema import Config
from ..core.errors import BackendError, ConfigError
from ..exec.process import Completed, diagnostic
from ..exec.ssh import SSH_FAILURE
from ..exec.transport import make_transport
from ..machines.registry import Machine
from ..parent.contract import load as load_contract
from ..parent.repos import load_repos

SANDBOX_HELP = "see the README section on sandbox dependencies"
USERNS_PROBE = "bwrap --unshare-user --unshare-net --ro-bind / / --dev /dev --proc /proc --die-with-parent -- /bin/true"
CODEX_PROTOCOL = ("turn/interrupt", "item/commandExecution/requestApproval", "outputSchema")
CLAUDE_FLAGS = ("--input-format", "--permission-prompts", "--json-schema", "--setting-sources", "--strict-mcp-config",
                "--append-system-prompt", "--session-id")
PARENT_FLAGS = {"claude": ("--json-schema", "--setting-sources", "--strict-mcp-config", "dontAsk"),
                "codex": ("--ignore-user-config", "--ignore-rules", "--output-schema", "--ephemeral")}


@dataclass(frozen=True)
class Check:
    status: str
    """PASS, FAIL, WARN, or SKIP"""
    name: str
    detail: str = ""


def passed(name: str) -> Check:
    return Check("PASS", name)


def failed(name: str, detail: str) -> Check:
    return Check("FAIL", name, detail)


class Probe:
    """Runs short shell commands on one machine, in its login shell for remote machines."""

    def __init__(self, machine: Machine, config: Config):
        self.machine = machine
        self.transport = make_transport(machine, excluded_env=(config.slack.app_token_env, config.slack.user_token_env))

    async def sh(self, script: str, *, timeout: float = 30) -> Completed:
        return await self.transport.probe(["sh", "-c", script], timeout=timeout)

    @property
    def where(self) -> str:
        return "locally" if self.machine.transport == "local" else f"on {self.machine.name}"


async def check_machine(config: Config, machine: Machine) -> list[Check]:
    label = f"Machine {machine.name} ({machine.transport}{': ' + machine.host if machine.host else ''})"
    if machine.transport == "slurm":
        return [Check("SKIP", label, "Slurm machines are registered but not supported yet; jobs there fail")]
    probe = Probe(machine, config)
    try:
        result = await probe.sh("uname -s")
    except (OSError, BackendError) as error:
        return [failed(label, f"could not run a command {probe.where}: {error}")]
    if result.returncode == SSH_FAILURE and machine.transport == "ssh":
        return [failed(label, f"could not connect without a prompt ({diagnostic(result.stderr) or 'ssh exited 255'});"
                              f" set up key authentication and check `ssh {machine.host}`")]
    system = result.text.strip()
    if result.returncode or system not in ("Linux", "Darwin"):
        return [failed(label, f"unsupported or unknown system {system or diagnostic(result.stderr)!r}")]
    checks = [passed(f"{label}, {system}")]
    for workspace in machine.workspaces:
        path = str(workspace.path)
        quoted = f'"$HOME"/{shlex.quote(path[2:])}' if path.startswith("~/") else shlex.quote(path)
        result = await probe.sh(f"test -d {quoted}")
        name = f"  workspace {workspace.name} ({path}, {workspace.policy.mode})"
        checks.append(passed(name) if result.returncode == 0 else failed(name, f"not a directory {probe.where}"))
    for backend in machine.backends:
        checks.extend(await check_backend(probe, backend, worker=True))
    checks.extend(await check_sandbox(probe, system, machine))
    return checks


async def check_backend(probe: Probe, backend: str, *, worker: bool) -> list[Check]:
    role = "worker" if worker else "parent"
    name = f"  {backend} ({role}) {probe.where}"
    found = await probe.sh(f"command -v {backend}")
    if found.returncode:
        return [failed(name, f"install {backend} and put it on the login shell's PATH {probe.where}")]
    checks = []
    if backend == "claude":
        help_text = (await probe.sh("claude --help")).text
        flags = CLAUDE_FLAGS if worker else PARENT_FLAGS["claude"]
        missing = [flag for flag in flags if flag not in help_text]
        checks.append(failed(name, f"upgrade claude; missing {', '.join(missing)}") if missing else passed(name + " capabilities"))
        status = await probe.sh("claude auth status")
        try:
            logged_in = json.loads(status.text).get("loggedIn") is True
        except (ValueError, AttributeError):
            logged_in = False
        checks.append(passed(name + " signed in") if status.returncode == 0 and logged_in
                      else failed(name, f"not signed in; run `claude` {probe.where} to sign in"))
    else:
        if worker:
            script = ('d=$(mktemp -d) || exit 2; codex app-server generate-json-schema --out "$d" >/dev/null 2>&1 || '
                      '{ rm -rf "$d"; exit 3; }; ' + " && ".join(f'grep -rq {shlex.quote(item)} "$d"' for item in CODEX_PROTOCOL)
                      + '; code=$?; rm -rf "$d"; exit $code')
            result = await probe.sh(script, timeout=60)
            checks.append(passed(name + " app-server protocol") if result.returncode == 0 else failed(
                name, "upgrade codex; its app-server lacks approvals, turn/interrupt, or outputSchema"))
            config_file = await probe.sh('grep -qs "^\\[mcp_servers" "$HOME/.codex/config.toml"')
            if config_file.returncode == 0:
                checks.append(Check("WARN", name, "~/.codex/config.toml defines MCP servers; codex app-server loads them "
                                                  "for workers (it cannot ignore the user config)"))
        else:
            help_text = (await probe.sh("codex exec --help")).text
            missing = [flag for flag in PARENT_FLAGS["codex"] if flag not in help_text]
            checks.append(failed(name, f"upgrade codex; missing {', '.join(missing)}") if missing else passed(name + " capabilities"))
        status = await probe.sh("codex login status")
        checks.append(passed(name + " signed in") if status.returncode == 0
                      else failed(name, f"not signed in; run `codex login` {probe.where}"))
    return checks


async def check_sandbox(probe: Probe, system: str, machine: Machine) -> list[Check]:
    """Both backends sandbox with bubblewrap on Linux; Claude also needs socat; gpu_confine uses bwrap directly."""
    if system != "Linux":
        return []
    name = f"  sandbox {probe.where}"
    needs = {"bwrap"} if ("codex" in machine.backends or any(item.policy.gpu_confine for item in machine.workspaces)) else set()
    if "claude" in machine.backends:
        needs |= {"bwrap", "socat"}
    missing = [tool for tool in sorted(needs) if (await probe.sh(f"command -v {tool}")).returncode]
    if missing:
        return [failed(name, f"install {', '.join(missing)} (sudo apt install bubblewrap socat); {SANDBOX_HELP}")]
    if not needs:
        return []
    result = await probe.sh(USERNS_PROBE)
    if result.returncode:
        return [failed(name, f"bubblewrap cannot create user namespaces ({diagnostic(result.stderr)}); on Ubuntu 24.04+"
                             f" add the AppArmor profile for bwrap; {SANDBOX_HELP}")]
    return [passed(name + " (bubblewrap user namespaces)")]


async def run_checks(path: Path) -> list[Check]:
    checks = [passed("Operating system") if sys.platform in ("linux", "darwin") else failed("Operating system", "macOS or Linux is required")]
    try:
        config = load_config(path)
    except ConfigError as error:
        return checks + [failed("Configuration", str(error)), Check("SKIP", "Everything else", "fix the configuration first")]
    checks.append(passed(f"Configuration ({config.path})"))
    try:
        load_contract(config.owner.contract)
        checks.append(passed(f"Agent contract ({config.owner.contract or 'packaged'})"))
    except ValueError as error:
        checks.append(failed("Agent contract", str(error)))
    try:
        repos = load_repos(config.parent.repos)
        checks.append(passed(f"Repository list ({len(repos)} repositories)"))
    except ValueError as error:
        checks.append(failed("Repository list", str(error)))
    for label, variable, prefix in (("Slack app token", config.slack.app_token_env, "xapp-"),
                                    ("Slack user token", config.slack.user_token_env, "xoxp-")):
        checks.append(passed(label) if os.environ.get(variable, "").startswith(prefix)
                      else failed(label, f"set {variable} to a {prefix} token"))
    local = next((machine for machine in config.machines.machines if machine.transport == "local"), None)
    parent_probe = Probe(local or Machine("local", "local", (), (config.parent.backend,), config.parent.backend,
                                          config.policy), config)
    checks.extend(await check_backend(parent_probe, config.parent.backend, worker=False))
    for group in await asyncio.gather(*(check_machine(config, machine) for machine in config.machines.machines)):
        checks.extend(group)
    return checks


def report(checks: list[Check]) -> int:
    color = sys.stdout.isatty() and not os.environ.get("NO_COLOR") and os.environ.get("TERM") != "dumb"
    colors = {"PASS": "32", "FAIL": "31", "WARN": "33", "SKIP": "33"}
    for check in checks:
        label = f"\033[1;{colors[check.status]}m{check.status}\033[0m" if color else check.status
        print(f"{label} {check.name}" + (f": {check.detail}" if check.detail else ""))
    counts = {status: sum(check.status == status for check in checks) for status in colors}
    print(f"Checks: {counts['PASS']} passed, {counts['FAIL']} failed, {counts['WARN']} warnings, {counts['SKIP']} skipped.")
    print("Slack authorization and channel membership are verified on start; no model request was made.")
    return 1 if counts["FAIL"] else 0
