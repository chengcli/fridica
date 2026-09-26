"""Read-only calls to the GitHub REST API.

Only the daemon calls GitHub. The optional token (``[github] token_env``) is read
from the daemon's environment and, like the Slack tokens, removed from every
child process's environment.
"""

from __future__ import annotations

import math
import time
from typing import Any, Protocol

API = "https://api.github.com"
TIMEOUT = 8.0
RATE_LIMIT_PAUSE = 300.0


class GitHubError(RuntimeError):
    """A request that did not return usable data; the message is safe to show the parent."""

    def __init__(self, message: str, *, retry_after: float = 0.0):
        super().__init__(message)
        self.retry_after = retry_after
        """Seconds to stop calling GitHub at all (rate limits); 0 for errors about one request."""


class GitHubAPI(Protocol):
    async def get(self, path: str) -> Any: ...


def retry_after(headers, now: float) -> float:
    """How long a rate limit lasts, from Retry-After or X-RateLimit-Reset; RATE_LIMIT_PAUSE when neither is usable."""
    for name, relative in (("Retry-After", True), ("X-RateLimit-Reset", False)):
        try:
            value = float(headers.get(name, ""))
        except (TypeError, ValueError):
            continue
        if math.isfinite(value):
            seconds = value if relative else value - now
            return min(max(seconds, 1.0), 3600.0)
    return RATE_LIMIT_PAUSE


class GitHubClient:
    def __init__(self, session, token: str = ""):
        self.session = session
        self.token = token

    async def get(self, path: str) -> Any:
        import aiohttp

        headers = {"Accept": "application/vnd.github+json", "X-GitHub-Api-Version": "2022-11-28",
                   "User-Agent": "fridica"}
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        try:
            async with self.session.get(API + path, headers=headers,
                                        timeout=aiohttp.ClientTimeout(total=TIMEOUT)) as response:
                return await self._read(response)
        except (aiohttp.ClientError, TimeoutError) as error:
            raise GitHubError(f"could not reach GitHub ({type(error).__name__})") from None

    @staticmethod
    async def _read(response) -> Any:
        if response.status == 404:
            raise GitHubError("not found (missing, or private without a token)")
        if response.status == 429 or (response.status == 403 and response.headers.get("X-RateLimit-Remaining") == "0"):
            raise GitHubError("rate limited; set [github] token_env for more requests",
                              retry_after=retry_after(response.headers, time.time()))
        if response.status >= 400:
            raise GitHubError(f"HTTP {response.status}")
        return await response.json()
