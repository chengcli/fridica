from __future__ import annotations

import os
from pathlib import Path
import shutil
import sys

from .agents import check_authentication, check_backend
from .config import load_config


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
        for name in ("Slack app token format", "Slack user token format", "AI executable", "AI CLI capabilities", "AI sign-in"):
            checks.append(("SKIP", f"{name}: fix the configuration first."))
    else:
        record("Configuration (identity, channels, workspace roots, and settings)", [])
        for label, variable, prefix in (
            ("Slack app token format", config.app_token_env, "xapp-"),
            ("Slack user token format", config.user_token_env, "xoxp-"),
        ):
            record(label, [] if os.environ.get(variable, "").startswith(prefix) else [f"Set {variable} to a {prefix} token."])
        executable = shutil.which(config.backend)
        record(f"AI executable ({config.backend})", [] if executable else [f"Install {config.backend} and ensure it is on PATH."])
        if executable:
            record("AI CLI capabilities", check_backend(config))
            record("AI sign-in", check_authentication(config))
        else:
            checks.extend(("SKIP", f"{name}: install the AI executable first.") for name in ("AI CLI capabilities", "AI sign-in"))
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
