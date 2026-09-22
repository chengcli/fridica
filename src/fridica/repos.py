"""The shared repository list, handed to the agent as data on every run.

``repos.toml`` ships inside the package and is the same for everyone; changes go
through a pull request to ``main``, where the test suite validates the file.
Each ``[[repos]]`` entry names a repository, its GitHub URL, and the people who
work on it; the first collaborator is the repository owner, whose word is final. Entries carry no local path: each person keeps checkouts wherever
they like, and the agent finds them under its workspace roots by git remote.
A ``repos`` path in ``config.toml`` overrides the shared list for local testing.
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
    notes: str = ""

    @property
    def owner(self) -> str:
        """The first collaborator: the person with the authoritative say on this repository."""
        return self.collaborators[0]

    def payload(self) -> dict:
        """The JSON-friendly form sent to the agent; the owner is spelled out so the model need not infer it."""
        data = asdict(self)
        data["collaborators"] = list(self.collaborators)
        data["owner"] = self.owner
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
        if not isinstance(entry, dict) or set(entry) - {"name", "url", "collaborators", "notes"}:
            raise ValueError(f"repository entry {index} has unknown fields; allowed: name, url, collaborators, notes "
                             "(local paths are deliberately not part of the shared list)")
        name, url = entry.get("name"), entry.get("url")
        if not isinstance(name, str) or not NAME.fullmatch(name.strip()):
            raise ValueError(f"repository entry {index} needs a name of letters, digits, dots, dashes, or spaces")
        if not isinstance(url, str) or not URL.fullmatch(url.strip()):
            raise ValueError(f"repository entry {index} ({name}) needs an https URL")
        if not isinstance(entry.get("notes", ""), str):
            raise ValueError(f"repository entry {index} ({name}): notes must be a string")
        collaborators = entry.get("collaborators")
        if not isinstance(collaborators, list) or not collaborators or any(not isinstance(c, str) or not c.strip() for c in collaborators):
            raise ValueError(f"repository entry {index} ({name}): collaborators must list at least the owner first, "
                             "as names or Slack IDs")
        label = name.strip().lower()
        if label in seen:
            raise ValueError(f"repository name '{label}' is listed twice")
        seen.add(label)
        repos.append(Repo(name.strip(), url.strip(), tuple(c.strip() for c in collaborators), entry.get("notes", "").strip()))
    return tuple(repos)


def load_repos(path: Path | None) -> tuple[Repo, ...]:
    """Parse the repository list at ``path``, or the shared packaged list when ``path`` is None."""
    if path is None:
        return parse_repos(default_repos_text())
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise ValueError(f"cannot read repository list {path}: {type(error).__name__}") from None
    try:
        return parse_repos(text)
    except ValueError as error:
        raise ValueError(f"{path}: {error}") from None
