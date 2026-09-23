from __future__ import annotations

import os
from pathlib import Path
import shutil
import sys

from .checks import check_authentication, check_backend, check_connection, check_host, check_sandbox, where, which
from .config import load_config
from .contract import load_contract
from .repos import load_repos


def run_doctor(path: Path) -> int:
    checks: list[tuple[str, str]] = []

    def record(name: str, problems: list[str]) -> None:
        checks.append(("FAIL" if problems else "PASS", name + (": " + " ".join(problems) if problems else "")))

    record("Supported operating system", [] if sys.platform in {"darwin", "linux"} else ["macOS or Linux is required."])
    try:
        config = load_config(path)
    except (ValueError, OSError, TypeError) as error:
        detail = str(error) if isinstance(error, ValueError) else type(error).__name__
        record("Configuration", [detail])
        for name in ("Agent contract", "Repository list", "Slack app token format", "Slack user token format",
                     "AI executable", "AI CLI capabilities", "AI sandbox", "AI sign-in"):
            checks.append(("SKIP", f"{name}: fix the configuration first."))
    else:
        record("Configuration (identity, channels, workspace roots, and settings)", [])
        connected = True
        if config.remote:
            problems = check_connection(config)
            connected = not problems
            record(f"SSH connection ({config.ssh_host}, workspace {config.root_label(config.workspace)})", problems)
        try:
            load_contract(config.contract)
        except ValueError as error:
            record("Agent contract", [str(error)])
        else:
            record(f"Agent contract ({config.contract or 'packaged default'})", [])
        try:
            repos = load_repos(config.repos)
        except ValueError as error:
            record("Repository list", [str(error)])
        else:
            record("Repository list" + (f" ({len(repos)}, local override at {config.repos})" if config.repos
                                        else f" ({len(repos)} shared with the package)"), [])
        for label, variable, prefix in (
            ("Slack app token format", config.app_token_env, "xapp-"),
            ("Slack user token format", config.user_token_env, "xoxp-"),
        ):
            record(label, [] if os.environ.get(variable, "").startswith(prefix) else [f"Set {variable} to a {prefix} token."])
        if not connected:
            checks.extend(("SKIP", f"{name}: fix the SSH connection first.")
                          for name in ("AI executable", "AI CLI capabilities", "AI sandbox", "AI sign-in"))
        else:
            executable = shutil.which(config.backend) if not config.remote else which(config, config.backend)
            record(f"AI executable ({config.backend}{', on ' + config.ssh_host if config.remote else ''})",
                   [] if executable else [f"Install {config.backend} and ensure it is on the {'login shell ' if config.remote else ''}PATH {where(config)}."])
            if executable:
                record("AI CLI capabilities", check_backend(config))
                record("AI sandbox", check_sandbox(config))
                record("AI sign-in", check_authentication(config))
            else:
                checks.extend(("SKIP", f"{name}: install the AI executable first.")
                              for name in ("AI CLI capabilities", "AI sandbox", "AI sign-in"))
        for host in config.remote_hosts:
            roots = ", ".join(host.label(root) for root in host.roots)
            record(f"Heavy-task host {host.name} ({roots})", check_host(config, host) if config.heavy_tasks
                   else [f"roots on {host.name} are only used by heavy tasks; set heavy_tasks = true or remove them."])
    color = sys.stdout.isatty() and not os.environ.get("NO_COLOR") and os.environ.get("TERM") != "dumb"
    colors = {"PASS": "32", "FAIL": "31", "SKIP": "33"}
    for status, description in checks:
        label = f"\033[1;{colors[status]}m{status}\033[0m" if color else status
        print(f"{label} {description}")
    passed = sum(status == "PASS" for status, _description in checks)
    failed = sum(status == "FAIL" for status, _description in checks)
    skipped = sum(status == "SKIP" for status, _description in checks)
    print(f"Checks: {passed} passed, {failed} failed, {skipped} skipped.")
    print("Slack authorization and channel membership are verified on start; no model request was made.")
    return 1 if failed or skipped else 0
