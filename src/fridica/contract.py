"""The editable rulebook every agent run reads before acting.

The contract is a Markdown file with two required sections. ``## Participation``
instructs the stateless classification call; ``## Replies`` instructs the call
that performs work and writes the Slack reply. Any further ``##`` section (for
example project-specific rules) is appended to the reply instruction with its
heading, so owners can organise rules without touching code. Prose before the
first heading is for people only. A packaged default ships with Fridica,
``fridica init`` copies it next to ``config.toml`` for editing, and the daemon
reloads it on every run so edits take effect without a restart.
"""
from __future__ import annotations

from dataclasses import dataclass
from importlib.resources import files
from pathlib import Path
import re

CONTRACT_LIMIT = 64 * 1024
SECTIONS = {"participation": ("participation", "classification"), "replies": ("replies", "response", "responses")}
HEADING = re.compile(r"^##\s+(.+?)\s*$", re.MULTILINE)


@dataclass(frozen=True)
class Contract:
    participation: str
    replies: str


def default_contract_text() -> str:
    return files("fridica").joinpath("contract.md").read_text(encoding="utf-8")


def parse_contract(text: str) -> Contract:
    if len(text.encode("utf-8")) > CONTRACT_LIMIT:
        raise ValueError(f"contract exceeds {CONTRACT_LIMIT // 1024} KiB")
    headings = list(HEADING.finditer(text))
    found: dict[str, str] = {}
    extra: list[str] = []
    for index, match in enumerate(headings):
        title = match.group(1).strip().lstrip("#").strip()
        name = title.lower()
        end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
        body = text[match.end():end].strip()
        key = next((key for key, aliases in SECTIONS.items() if name in aliases), None)
        if key is None:
            if body:
                extra.append(f"## {title}\n\n{body}")
        elif key not in found:
            found[key] = body
    missing = [key for key in SECTIONS if not found.get(key)]
    if missing:
        raise ValueError(
            "contract is missing or has an empty section: " + ", ".join(f"## {key.capitalize()}" for key in missing)
        )
    return Contract(found["participation"], "\n\n".join([found["replies"], *extra]))


def load_contract(path: Path | None) -> Contract:
    """Parse the contract at ``path``, or the packaged default when ``path`` is None."""
    if path is None:
        return parse_contract(default_contract_text())
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"cannot read contract {path}: {type(error).__name__}") from None
    try:
        return parse_contract(text)
    except ValueError as error:
        raise ValueError(f"{path}: {error}") from None
