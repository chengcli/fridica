from __future__ import annotations

import math
import os
from pathlib import Path
import re
import tomllib
import tempfile
import unicodedata
from dataclasses import dataclass

import tomlkit


DEFAULT_CONFIG = Path.home() / ".config" / "fridica" / "config.toml"
DEFAULT_STATE = Path.home() / ".local" / "state" / "fridica" / "state.sqlite3"


def within_casefold(path: Path, root: Path) -> bool:
    """Compare deny boundaries conservatively on case-insensitive filesystems."""
    return Path(unicodedata.normalize('NFD', str(path)).casefold()).is_relative_to(
        Path(unicodedata.normalize('NFD', str(root)).casefold()))


@dataclass(frozen=True)
class Config:
    owner_id: str
    workspace_id: str
    channels: tuple[str, ...]
    workspace: Path
    additional_workspaces: tuple[Path, ...] = ()
    backend: str = "claude"
    model: str | None = None
    reasoning_effort: str | None = None
    max_wait_replies: int = 3
    profile: str = ""
    state_path: Path = DEFAULT_STATE
    app_token_env: str = "SLACK_APP_TOKEN"
    user_token_env: str = "SLACK_USER_TOKEN"
    timeout: float = 600
    context_limit: int = 50
    cooldown: float = 60
    max_turns: int = 6
    general_messages: bool = True
    resume_sessions: bool = True
    session_timeout: float = 14 * 86400
    contract: Path | None = None
    file_access: bool = False
    read_only_workspaces: tuple[Path, ...] = ()

    def __post_init__(self) -> None:
        for label, value, pattern in (
            ("owner_id", self.owner_id, r"[UW][A-Z0-9]+"),
            ("workspace_id", self.workspace_id, r"T[A-Z0-9]+"),
        ):
            if not isinstance(value, str) or not re.fullmatch(pattern, value):
                raise ValueError(f"{label} must be a Slack ID")
        if not isinstance(self.channels, (list, tuple)) or not self.channels:
            raise ValueError("channels must explicitly list allowed Slack channels")
        if any(not isinstance(channel, str) or not re.fullmatch(r"[CG][A-Z0-9]+", channel) for channel in self.channels):
            raise ValueError("channels must contain Slack channel IDs")
        if self.backend not in ("claude", "codex"):
            raise ValueError("backend must be claude or codex")
        if not self.state_path.is_absolute():
            raise ValueError("state_path must be an absolute path")
        for name in ("timeout", "cooldown", "context_limit", "max_turns", "max_wait_replies", "session_timeout"):
            value = getattr(self, name)
            minimum = 0 if name in {"cooldown", "session_timeout"} else 1
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < minimum:
                raise ValueError(f"{name} must be a finite number >= {minimum}")
            if name in {"context_limit", "max_turns", "max_wait_replies"} and not isinstance(value, int):
                raise ValueError(f"{name} must be an integer")
        for name in ("general_messages", "resume_sessions", "file_access"):
            if not isinstance(getattr(self, name), bool):
                raise ValueError(f"{name} must be boolean")
        if not isinstance(self.profile, str) or (self.model is not None and not isinstance(self.model, str)):
            raise ValueError("profile and model must be strings")
        if self.reasoning_effort not in {None, "low", "medium", "high", "xhigh", "max", "ultra"}:
            raise ValueError("invalid reasoning_effort")
        if self.contract is not None and (not isinstance(self.contract, Path) or not self.contract.is_absolute()
                                          or not self.contract.is_file()):
            raise ValueError("contract must be an existing Markdown file")
        for name in ("app_token_env", "user_token_env"):
            value = getattr(self, name)
            if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
                raise ValueError(f"{name} must name an environment variable")
        if self.read_only_workspaces and not self.file_access:
            raise ValueError("read_only_workspaces requires file_access")
        for directory in (self.workspace, *self.additional_workspaces, *self.read_only_workspaces):
            if not directory.is_absolute() or not directory.is_dir():
                raise ValueError("workspace roots must be existing absolute directories")
            if directory.samefile(Path.home()) or directory.samefile(Path("/")):
                raise ValueError("choose a project directory, not the home or filesystem root")
        if self.file_access:
            package = Path(__file__).resolve().parent
            for directory in (self.workspace, *self.additional_workspaces):
                if within_casefold(package, directory.resolve()) or within_casefold(directory.resolve(), package):
                    raise ValueError("Fridica's installed code must be outside writable roots")
        if within_casefold(self.state_path.resolve(), self.workspace.resolve()) or any(
            within_casefold(self.state_path.resolve(), directory.resolve()) for directory in (*self.additional_workspaces, *self.read_only_workspaces)
        ):
            raise ValueError("state_path must be outside agent workspaces")

    def tokens(self) -> tuple[str, str]:
        app = os.environ.get(self.app_token_env, "")
        user = os.environ.get(self.user_token_env, "")
        if not app.startswith("xapp-"):
            raise ValueError(f"set {self.app_token_env} to a Slack app-level token")
        if not user.startswith("xoxp-"):
            raise ValueError(f"set {self.user_token_env} to a Slack user token")
        return app, user


def set_slack_ids(path: Path, *, owner_id: str | None = None,
                  workspace_id: str | None = None, channels: list[str] | None = None) -> None:
    updates = {}
    for name, value, pattern in (
        ("owner_id", owner_id, r"[UW][A-Z0-9]+"),
        ("workspace_id", workspace_id, r"T[A-Z0-9]+"),
    ):
        if value is not None:
            if not re.fullmatch(pattern, value):
                raise ValueError(f"{name} must be a Slack ID")
            updates[name] = value
    if channels is not None:
        if not channels or any(not re.fullmatch(r"[CG][A-Z0-9]+", channel) for channel in channels):
            raise ValueError("channels must contain Slack channel IDs")
        updates["channels"] = list(dict.fromkeys(channels))
    if not updates:
        raise ValueError("provide --owner-id, --workspace-id, or --channel-id")
    path = path.expanduser()
    if path.is_symlink():
        raise ValueError("refusing to replace a symlink configuration; specify its target path")
    if not path.is_file():
        raise ValueError("configuration not found; run fridica init with the same --config path first")
    original = path.read_text()
    try:
        document = tomlkit.parse(original)
    except ValueError:
        raise ValueError("configuration is not valid TOML; repair it before updating IDs") from None
    for name, value in updates.items():
        document[name] = value
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=path.parent,
                                         prefix=".fridica-config-", delete=False) as stream:
            temporary = Path(stream.name)
            stream.write(tomlkit.dumps(document))
            stream.flush()
            os.fsync(stream.fileno())
        if path.is_symlink() or path.read_text() != original:
            raise ValueError("configuration changed during update; retry with the latest file")
        os.replace(temporary, path)
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def load_config(path: Path, *, contents: bytes | None = None) -> Config:
    values = tomllib.loads((path.expanduser().read_bytes() if contents is None else contents).decode("utf-8"))
    allowed = set(Config.__dataclass_fields__)
    if set(values) - allowed:
        raise ValueError("unknown configuration fields: " + ", ".join(sorted(set(values) - allowed)))
    for required in ("owner_id", "workspace_id", "channels", "workspace"):
        if required not in values:
            raise ValueError(f"missing configuration field: {required}")
    for key in ("workspace", "state_path"):
        if key in values:
            if not isinstance(values[key], str) or not values[key]:
                raise ValueError(f"{key} must be a nonempty path")
            values[key] = Path(values[key]).expanduser()
    for key in ("additional_workspaces", "read_only_workspaces"):
        roots = values.get(key, [])
        if not isinstance(roots, list) or any(not isinstance(root, str) or not root for root in roots):
            raise ValueError(f"{key} must be a list of paths")
        values[key] = tuple(Path(root).expanduser() for root in roots)
    if "contract" in values:
        if not isinstance(values["contract"], str) or not values["contract"]:
            raise ValueError("contract must be a nonempty path")
        contract = Path(values["contract"]).expanduser()
        values["contract"] = contract if contract.is_absolute() else (path.expanduser().parent / contract).resolve()
    elif (path.expanduser().parent / "contract.md").is_file():
        values["contract"] = path.expanduser().parent / "contract.md"
    config = Config(**values)
    if config.file_access:
        roots = (config.workspace, *config.additional_workspaces, *config.read_only_workspaces)
        for protected in (path.expanduser(), config.contract):
            if protected and any(within_casefold(protected.resolve(), root.resolve()) for root in roots):
                raise ValueError("configuration and contract must be outside file access roots")
    return config


TEMPLATE = '''# Create your own Slack app using slack/manifest.yaml.
owner_id = "U_REPLACE"
workspace_id = "T_REPLACE"
channels = ["C_REPLACE"]
workspace = "~/projects/your-project"
additional_workspaces = []
# Use scoped file operations instead of the agent's native workspace tools.
file_access = false
read_only_workspaces = []
backend = "claude"
# model = "your-preferred-model"
# reasoning_effort = "low"  # Codex only
profile = "My projects and expertise: ..."
app_token_env = "SLACK_APP_TOKEN"
user_token_env = "SLACK_USER_TOKEN"
general_messages = true
context_limit = 50
timeout = 600
cooldown = 60
max_turns = 6
max_wait_replies = 3
resume_sessions = true
# Seconds of thread inactivity after which a stored session is not resumed (2 weeks).
session_timeout = 1209600
# Agent rules live in contract.md beside this file (created by fridica init).
# Uncomment to use a different file; relative paths resolve from this directory.
# contract = "contract.md"
'''
