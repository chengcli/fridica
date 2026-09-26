"""Shared helpers for the design-document build: paths, read-only snapshots, anonymizing, RST output.

Nothing here writes to Fridica's databases. Each database is copied through the SQLite backup API into a
temporary snapshot, so a running daemon is never blocked and every figure and table in one build comes from
the same instant.
"""

from __future__ import annotations

from contextlib import contextmanager
from pathlib import Path
import re
import sqlite3
import string
import tempfile

DOC = Path(__file__).resolve().parents[1]
REPO = DOC.parent
FIGURES = DOC / "figures"
RST = DOC / "rst"
GENERATED = RST / "generated"
DEFAULT_STATE = Path("~/.local/state/fridica/state.sqlite3").expanduser()
DEFAULT_LEGACY = Path("~/.local/state/fridica/state.sqlite3.bak").expanduser()

# Slack user, bot, channel, group, and team IDs; none may appear in anything the build writes.
SLACK_ID = re.compile(r"\b[UWCGTB][A-Z0-9]{8,}\b")


@contextmanager
def snapshot(path: Path):
    """A private, consistent read-only copy of a SQLite database (WAL included)."""
    source = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    with tempfile.TemporaryDirectory(prefix="fridica-doc-") as directory:
        copy = sqlite3.connect(Path(directory) / "snapshot.sqlite3")
        try:
            source.backup(copy)
            copy.row_factory = sqlite3.Row
            yield copy
        finally:
            copy.close()
            source.close()


class Anonymizer:
    """Stable pseudonyms: the owner, person A, B, …, and channel 1, 2, …"""

    def __init__(self, owner: str = ""):
        self.owner = owner
        self.people: dict[str, str] = {}
        self.channels: dict[str, str] = {}
        self.machines: dict[str, str] = {}

    def person(self, identifier: str) -> str:
        if identifier == self.owner:
            return "owner"
        if identifier not in self.people:
            index = len(self.people)
            letters = string.ascii_uppercase
            label = letters[index] if index < 26 else letters[index // 26 - 1] + letters[index % 26]
            self.people[identifier] = f"person {label}"
        return self.people[identifier]

    def channel(self, identifier: str) -> str:
        if identifier not in self.channels:
            self.channels[identifier] = f"channel {len(self.channels) + 1}"
        return self.channels[identifier]

    def machine(self, name: str) -> str:
        if name not in self.machines:
            self.machines[name] = f"Node {len(self.machines) + 1}"
        return self.machines[name]


def assert_anonymous(text: str, where: str) -> None:
    found = sorted(set(SLACK_ID.findall(text)))
    if found:
        raise SystemExit(f"privacy check failed: Slack IDs {found[:3]}… in {where}")


def number(value, digits: int = 0) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, float) and digits:
        return f"{value:,.{digits}f}"
    return f"{round(value):,}" if isinstance(value, (int, float)) else str(value)


def list_table(title: str, header: list[str], rows: list[list], widths: list[int] | None = None) -> str:
    """An RST list-table; every cell is converted to text."""
    lines = [f".. list-table:: {title}", "   :header-rows: 1"]
    if widths:
        lines.append("   :widths: " + " ".join(str(width) for width in widths))
    lines.append("")
    for row in [header, *rows]:
        cells = [str(cell).replace("\n", " ") if str(cell) else " " for cell in row]
        lines.append(f"   * - {cells[0]}")
        lines.extend(f"     - {cell}" for cell in cells[1:])
    return "\n".join(lines) + "\n"


def substitutions(values: dict[str, object]) -> str:
    """``.. |name| replace:: value`` lines so the prose can cite numbers without hard-coding them."""
    return "\n".join(f".. |{name}| replace:: {value}" for name, value in sorted(values.items())) + "\n"
