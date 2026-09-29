"""Time source used by the daemon, replaceable in tests."""

from __future__ import annotations

import asyncio
import time


class Clock:
    def now(self) -> float:
        return time.time()

    async def sleep(self, seconds: float) -> None:
        await asyncio.sleep(seconds)


class ReplayClock(Clock):
    """Driven by a corpus's event times; sleeping only yields to other tasks."""
    def __init__(self, now: float = 0.0):
        self.value = now

    def now(self) -> float:
        return self.value

    def set(self, now: float) -> None:
        self.value = now

    async def sleep(self, seconds: float) -> None:
        await asyncio.sleep(0)
