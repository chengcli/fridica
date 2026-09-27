"""In-process doorbells.

SQLite is the queue: every unit of work (an inbox row, an outbox row, a queued job)
is committed before anyone is told about it. A doorbell only wakes the component
that should look, so a lost ring costs latency, never work.
"""

from __future__ import annotations

import asyncio
from collections.abc import Callable


class Doorbell:
    def __init__(self) -> None:
        self._event = asyncio.Event()

    def ring(self) -> None:
        self._event.set()

    async def wait(self, timeout: float | None = None) -> bool:
        """Wait for a ring (or the timeout); return whether it rang, and re-arm."""
        try:
            await asyncio.wait_for(self._event.wait(), timeout)
            rang = True
        except TimeoutError:
            rang = False
        self._event.clear()
        return rang


class Bus:
    def __init__(self) -> None:
        self.outbox = Doorbell()
        self.jobs = Doorbell()
        self._thread_listeners: list[Callable[[str], None]] = []

    def on_thread(self, listener: Callable[[str], None]) -> None:
        self._thread_listeners.append(listener)

    def thread(self, session_id: str) -> None:
        """Announce that a thread session has new inbox work."""
        for listener in self._thread_listeners:
            listener(session_id)
