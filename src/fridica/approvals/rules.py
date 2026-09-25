"""Decisions policy can make without asking the owner."""

from __future__ import annotations

import re

from ..machines.registry import Policy
from ..workers.protocol import ALLOW_ONCE, DENY, ApprovalRequest

# A prefix rule such as "pytest" must not approve "pytest; rm -rf ~" or "pytest $(curl …)".
SHELL_CONTROL = re.compile(r"[;&|`$<>(){}\n\\]")


def command_of(request: ApprovalRequest) -> str:
    command = request.detail.get("command")
    if command is None and isinstance(request.detail.get("input"), dict):
        command = request.detail["input"].get("command")
    return command.strip() if isinstance(command, str) else ""


def matches(command: str, prefix: str) -> bool:
    return command == prefix or command.startswith(prefix + " ")


def decide(policy: Policy, request: ApprovalRequest) -> str | None:
    """ALLOW_ONCE or DENY when a rule applies, otherwise None (ask the owner)."""
    if request.kind != "command":
        return None
    command = command_of(request)
    if not command:
        return None
    if any(matches(command, prefix) for prefix in policy.auto_deny):
        return DENY
    if SHELL_CONTROL.search(command):
        return None
    if any(matches(command, prefix) for prefix in policy.auto_approve):
        return ALLOW_ONCE
    return None
