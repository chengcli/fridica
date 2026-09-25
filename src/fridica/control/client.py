"""A small client for the daemon's control socket."""

from __future__ import annotations

import asyncio
from pathlib import Path
from typing import Any

import aiohttp


class DaemonUnavailable(RuntimeError):
    pass


class ControlError(RuntimeError):
    def __init__(self, status: int, message: str):
        super().__init__(message)
        self.status = status


class ControlClient:
    def __init__(self, socket: Path):
        self.socket = socket

    async def request(self, method: str, path: str, body: dict | None = None, *, timeout: float = 30) -> Any:
        if not self.socket.exists():
            raise DaemonUnavailable(f"fridica is not running (no control socket at {self.socket})")
        connector = aiohttp.UnixConnector(path=str(self.socket))
        try:
            async with aiohttp.ClientSession(connector=connector, timeout=aiohttp.ClientTimeout(total=timeout)) as session:
                async with session.request(method, f"http://fridica{path}", json=body) as response:
                    if response.status >= 400:
                        raise ControlError(response.status, (await response.text()).strip())
                    return await response.json()
        except aiohttp.ClientConnectionError as error:
            raise DaemonUnavailable(f"fridica is not responding on {self.socket}: {error}") from None

    def call(self, method: str, path: str, body: dict | None = None, **options) -> Any:
        """Synchronous wrapper for the CLI."""
        return asyncio.run(self.request(method, path, body, **options))
