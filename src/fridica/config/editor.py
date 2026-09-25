"""Edit config.toml in place, keeping comments, and only after the result validates."""

from __future__ import annotations

import os
from pathlib import Path
import tempfile

import tomlkit

from .loader import load_config
from .schema import Config


def update(path: Path, changes: dict[str, dict], *, validate: bool = True) -> Config | None:
    """Apply ``{"table": {"key": value}}`` changes (dotted table names allowed) and return the new config.

    The edited text is validated with the real loader from a temporary file beside
    the original, then atomically replaces it, so a rejected edit leaves the file
    untouched. ``validate=False`` is for ``fridica configure``, which fills in Slack
    IDs before the rest of a fresh configuration is complete; it returns None.
    """
    path = path.expanduser()
    document = tomlkit.parse(path.read_text(encoding="utf-8"))
    for table_name, values in changes.items():
        table = document
        for part in table_name.split("."):
            if part not in table:
                table[part] = tomlkit.table()
            table = table[part]
        for key, value in values.items():
            table[key] = value
    text = tomlkit.dumps(document)
    descriptor, temporary = tempfile.mkstemp(dir=path.parent, prefix=".config-", suffix=".toml")
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            stream.write(text)
        os.chmod(temporary, 0o600)
        if validate:
            load_config(Path(temporary))
        os.replace(temporary, path)
    except BaseException:
        Path(temporary).unlink(missing_ok=True)
        raise
    return load_config(path) if validate else None
