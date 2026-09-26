"""Typed configuration. Defaults live here and nowhere else."""

from __future__ import annotations

from dataclasses import dataclass, field
import os
from pathlib import Path

from ..core.errors import ConfigError
from ..machines.registry import Policy, Registry

DEFAULT_CONFIG = Path.home() / ".config" / "fridica" / "config.toml"
DEFAULT_STATE = Path.home() / ".local" / "state" / "fridica" / "state.sqlite3"
REASONING_EFFORTS = ("", "low", "medium", "high", "xhigh", "max")


@dataclass(frozen=True)
class OwnerConfig:
    slack_user: str
    profile: str = ""
    contract: Path | None = None
    """None means the contract packaged with Fridica."""


@dataclass(frozen=True)
class SlackConfig:
    workspace: str
    channels: tuple[str, ...]
    delegate_channels: tuple[str, ...] | None = None
    """Channels whose members may start worker jobs; None means every configured channel."""
    app_token_env: str = "SLACK_APP_TOKEN"
    user_token_env: str = "SLACK_USER_TOKEN"
    general_messages: bool = True
    """Consider replying to messages that do not mention the owner (a triage call decides)."""
    cooldown: float = 60.0

    def may_delegate(self, channel: str) -> bool:
        return self.delegate_channels is None or channel in self.delegate_channels


@dataclass(frozen=True)
class ParentConfig:
    backend: str = "claude"
    model: str = ""
    triage_model: str = ""
    reasoning_effort: str = ""
    timeout: float = 180.0
    context_chars: int = 24000
    default_machine: str = ""
    repos: Path | None = None
    """None means the shared repository list packaged with Fridica."""


@dataclass(frozen=True)
class Limits:
    max_wait_replies: int = 3
    max_no_progress: int = 3
    max_delegations_per_turn: int = 3
    max_workers_per_thread: int = 4
    max_jobs: int = 4
    parent_concurrency: int = 4
    job_timeout: float = 4 * 3600.0
    worker_idle: float = 1800.0
    session_timeout: float = 14 * 86400.0
    """Threads quiet for longer start their workers fresh instead of resuming backend sessions."""
    auto_resume: bool = False
    reply_chars: int = 7000
    report_fast_path: bool = True
    """Post a single finished worker's report directly instead of asking the parent to rewrite it."""


@dataclass(frozen=True)
class GitHubConfig:
    enabled: bool = True
    """Show the parent the current state of GitHub pull requests and issues linked in a thread."""
    token_env: str = "FRIDICA_GITHUB_TOKEN"
    """Optional read-only token; unset means anonymous requests (60 per hour per IP)."""
    cache_seconds: float = 180.0
    """How long a pull request's or issue's state is reused before it is fetched again."""


@dataclass(frozen=True)
class StateConfig:
    path: Path = DEFAULT_STATE
    control_socket: Path = DEFAULT_STATE.with_name("control.sock")


@dataclass(frozen=True)
class Config:
    owner: OwnerConfig
    slack: SlackConfig
    machines: Registry
    parent: ParentConfig = ParentConfig()
    limits: Limits = Limits()
    policy: Policy = Policy()
    state: StateConfig = StateConfig()
    github: GitHubConfig = GitHubConfig()
    path: Path | None = None
    fingerprint: str = field(default="", compare=False)

    def secret_env(self) -> tuple[str, ...]:
        """Environment variables that hold the daemon's credentials; no child process may see them."""
        return (self.slack.app_token_env, self.slack.user_token_env, self.github.token_env)

    def tokens(self) -> tuple[str, str]:
        """The Socket Mode app token and the owner's user token, read from the environment."""
        app = os.environ.get(self.slack.app_token_env, "")
        user = os.environ.get(self.slack.user_token_env, "")
        if not app.startswith("xapp-"):
            raise ConfigError(f"{self.slack.app_token_env} must hold a Slack app-level token (xapp-...)")
        if not user.startswith("xoxp-"):
            raise ConfigError(f"{self.slack.user_token_env} must hold a Slack user token (xoxp-...)")
        return app, user
