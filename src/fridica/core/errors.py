"""Errors shared across layers."""

from __future__ import annotations


class ConfigError(ValueError):
    """The configuration file is invalid; the message names the offending key."""


class BackendError(RuntimeError):
    """A Claude or Codex process failed; the message is safe for the local log, never for Slack."""


class SessionUnavailable(BackendError):
    """A backend could not resume a stored session (expired, deleted, or on another machine)."""


class MatchError(ValueError):
    """A delegation selector matched no machine or workspace, or matched ambiguously."""

    def __init__(self, message: str, candidates: tuple[str, ...] = ()):
        super().__init__(message)
        self.candidates = candidates


class RateLimited(RuntimeError):
    """Slack asked us to slow down; retry after ``retry_after`` seconds."""

    def __init__(self, retry_after: float):
        super().__init__(f"rate limited for {retry_after:g}s")
        self.retry_after = retry_after


class DeliveryRejected(RuntimeError):
    """Slack refused a post permanently (bad channel, missing scope, message too long)."""


class DeliveryAmbiguous(RuntimeError):
    """A post may or may not have reached Slack (5xx, connection reset); never resend automatically."""
