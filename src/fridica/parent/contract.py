"""The owner-editable rulebook for the parent, the triage call, and every worker."""

from __future__ import annotations

from dataclasses import dataclass
from importlib.resources import files
from pathlib import Path
import re

CONTRACT_LIMIT = 64 * 1024
SECTIONS = {
    "participation": ("participation", "triage"),
    "replies": ("replies", "reply"),
    "delegation": ("delegation",),
    "workers": ("worker reports", "workers", "worker"),
    "debriefs": ("debriefs", "debrief"),
}
REQUIRED = ("participation", "replies")
HEADING = re.compile(r"^##\s+(.+?)\s*$", re.MULTILINE)


@dataclass(frozen=True)
class Contract:
    participation: str
    replies: str
    delegation: str = ""
    workers: str = ""
    debriefs: str = ""
    extra: str = ""
    """Every other section, with its heading; given to the parent and to workers."""

    @property
    def parent(self) -> str:
        return "\n\n".join(part for part in (self.replies, self.delegation, self.extra) if part)

    @property
    def worker(self) -> str:
        return "\n\n".join(part for part in (self.workers, self.extra) if part)


def default_text() -> str:
    return files("fridica.parent").joinpath("contract.md").read_text(encoding="utf-8")


def parse(text: str) -> Contract:
    if len(text.encode("utf-8")) > CONTRACT_LIMIT:
        raise ValueError(f"contract exceeds {CONTRACT_LIMIT // 1024} KiB")
    headings = list(HEADING.finditer(text))
    found: dict[str, str] = {}
    extra: list[str] = []
    for index, match in enumerate(headings):
        title = match.group(1).strip().lstrip("#").strip()
        end = headings[index + 1].start() if index + 1 < len(headings) else len(text)
        body = text[match.end():end].strip()
        key = next((key for key, aliases in SECTIONS.items() if title.lower() in aliases), None)
        if key is None:
            if body:
                extra.append(f"## {title}\n\n{body}")
        elif key not in found:
            found[key] = body
    missing = [key for key in REQUIRED if not found.get(key)]
    if missing:
        raise ValueError("contract is missing or has an empty section: " + ", ".join(f"## {key.capitalize()}" for key in missing))
    return Contract(extra="\n\n".join(extra), **{key: found.get(key, "") for key in SECTIONS})


def load(path: Path | None) -> Contract:
    """The contract at ``path`` (or the packaged one), with packaged defaults for missing optional sections."""
    default = parse(default_text())
    if path is None:
        return default
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"cannot read contract {path}: {type(error).__name__}") from None
    try:
        contract = parse(text)
    except ValueError as error:
        raise ValueError(f"{path}: {error}") from None
    return Contract(participation=contract.participation, replies=contract.replies,
                    delegation=contract.delegation or default.delegation, workers=contract.workers or default.workers,
                    debriefs=contract.debriefs or default.debriefs,
                    extra=contract.extra)
