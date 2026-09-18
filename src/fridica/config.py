from __future__ import annotations

import math
import os
from pathlib import Path
import re
import tomllib
from dataclasses import dataclass


DEFAULT_CONFIG = Path.home() / ".config" / "fridica" / "config.toml"
DEFAULT_STATE = Path.home() / ".local" / "state" / "fridica" / "state.sqlite3"


@dataclass(frozen=True)
class Config:
    owner_id: str
    workspace_id: str
    channels: tuple[str, ...]
    workspace: Path
    additional_workspaces: tuple[Path, ...] = ()
    backend: str = "claude"
    model: str | None = None
    profile: str = ""
    state_path: Path = DEFAULT_STATE
    app_token_env: str = "FRIDICA_SLACK_APP_TOKEN"
    user_token_env: str = "FRIDICA_SLACK_USER_TOKEN"
    timeout: float = 600
    context_limit: int = 50
    cooldown: float = 60
    max_turns: int = 6
    general_messages: bool = True

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
        for name in ("timeout", "cooldown", "context_limit", "max_turns"):
            value = getattr(self, name)
            minimum = 0 if name == "cooldown" else 1
            if isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < minimum:
                raise ValueError(f"{name} must be a finite number >= {minimum}")
            if name in {"context_limit", "max_turns"} and not isinstance(value, int):
                raise ValueError(f"{name} must be an integer")
        if not isinstance(self.general_messages, bool):
            raise ValueError("general_messages must be boolean")
        if not isinstance(self.profile, str) or (self.model is not None and not isinstance(self.model, str)):
            raise ValueError("profile and model must be strings")
        for name in ("app_token_env", "user_token_env"):
            value = getattr(self, name)
            if not isinstance(value, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", value):
                raise ValueError(f"{name} must name an environment variable")
        for directory in (self.workspace, *self.additional_workspaces):
            if not directory.is_absolute() or not directory.is_dir():
                raise ValueError("workspace roots must be existing absolute directories")
            if directory.resolve() in {Path.home().resolve(), Path("/")}:
                raise ValueError("choose a project directory, not the home or filesystem root")
        if self.state_path.resolve().is_relative_to(self.workspace.resolve()) or any(
            self.state_path.resolve().is_relative_to(directory.resolve()) for directory in self.additional_workspaces
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


def load_config(path: Path) -> Config:
    with path.expanduser().open("rb") as stream:
        values = tomllib.load(stream)
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
    roots = values.get("additional_workspaces", [])
    if not isinstance(roots, list) or any(not isinstance(root, str) for root in roots):
        raise ValueError("additional_workspaces must be a list of paths")
    values["additional_workspaces"] = tuple(Path(root).expanduser() for root in roots)
    return Config(**values)


TEMPLATE = '''# Create your own Slack app using slack/manifest.yaml.
owner_id = "U_REPLACE"
workspace_id = "T_REPLACE"
channels = ["C_REPLACE"]
workspace = "~/projects/your-project"
additional_workspaces = []
backend = "claude"
# model = "your-preferred-model"
profile = "My projects and expertise: ..."
app_token_env = "FRIDICA_SLACK_APP_TOKEN"
user_token_env = "FRIDICA_SLACK_USER_TOKEN"
general_messages = true
context_limit = 50
timeout = 600
cooldown = 60
max_turns = 6
'''
