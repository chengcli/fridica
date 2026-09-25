"""Time source used by the daemon, replaceable in tests."""

from __future__ import annotations

import asyncio
import time


class Clock:
    def now(self) -> float:
        return time.time()

    async def sleep(self, seconds: float) -> None:
        await asyncio.sleep(seconds)
