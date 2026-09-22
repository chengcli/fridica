"""The owner's editable list of repositories, handed to the agent as data.

``repos.toml`` lives beside ``config.toml``. Each ``[[repos]]`` entry names a
repository, its collaborators, GitHub URL, and optional local path.
The agent receives the list on every run and uses it to resolve which
repository a request refers to; the contract tells it to ask when the choice is
ambiguous. This module only parses and validates the file.
"""
from __future__ import annotations

from dataclasses import asdict, dataclass, field
from importlib.resources import files
from pathlib import Path
import re
import tomllib

REPOS_LIMIT = 64 * 1024
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._ -]{0,63}")
URL = re.compile(r"https://[A-Za-z0-9.-]+/[^\s]+")


@dataclass(frozen=True)
class Repo:
    name: str
    url: str
    collaborators: tuple[str, ...] = ()
    path: str = ""
    notes: str = ""

    def payload(self) -> dict:
        """The JSON-friendly form sent to the agent; empty optional fields are omitted."""
        data = asdict(self)
        data["collaborators"] = list(self.collaborators)
        return {key: value for key, value in data.items() if value not in ("", [])}


def default_repos_text() -> str:
    return files("fridica").joinpath("repos.toml").read_text(encoding="utf-8")


def parse_repos(text: str) -> tuple[Repo, ...]:
    if len(text.encode("utf-8")) > REPOS_LIMIT:
        raise ValueError(f"repository list exceeds {REPOS_LIMIT // 1024} KiB")
    try:
        document = tomllib.loads(text)
    except tomllib.TOMLDecodeError as error:
        raise ValueError(f"repository list is not valid TOML: {error}") from None
    entries = document.get("repos", [])
    if set(document) - {"repos"} or not isinstance(entries, list):
        raise ValueError("repository list must contain only [[repos]] entries")
    repos: list[Repo] = []
    seen: set[str] = set()
    for index, entry in enumerate(entries, 1):
        if not isinstance(entry, dict) or set(entry) - {"name", "url", "collaborators", "path", "notes"}:
            raise ValueError(f"repository entry {index} has unknown fields; allowed: name, url, collaborators, path, notes")
        name, url = entry.get("name"), entry.get("url")
        if not isinstance(name, str) or not NAME.fullmatch(name.strip()):
            raise ValueError(f"repository entry {index} needs a name of letters, digits, dots, dashes, or spaces")
        if not isinstance(url, str) or not URL.fullmatch(url.strip()):
            raise ValueError(f"repository entry {index} ({name}) needs an https URL")
        for key in ("path", "notes"):
            if not isinstance(entry.get(key, ""), str):
                raise ValueError(f"repository entry {index} ({name}): {key} must be a string")
        collaborators = entry.get("collaborators", [])
        if not isinstance(collaborators, list) or any(not isinstance(c, str) or not c.strip() for c in collaborators):
            raise ValueError(f"repository entry {index} ({name}): collaborators must be a list of names or Slack IDs")
        label = name.strip().lower()
        if label in seen:
            raise ValueError(f"repository name '{label}' is listed twice")
        seen.add(label)
        repos.append(Repo(name.strip(), url.strip(), tuple(c.strip() for c in collaborators),
                          entry.get("path", "").strip(), entry.get("notes", "").strip()))
    return tuple(repos)


def load_repos(path: Path | None) -> tuple[Repo, ...]:
    """Parse the repository list at ``path``; None means no list is configured."""
    if path is None:
        return ()
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"cannot read repository list {path}: {type(error).__name__}") from None
    try:
        return parse_repos(text)
    except ValueError as error:
        raise ValueError(f"{path}: {error}") from None
