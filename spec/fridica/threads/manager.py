"""Starts one actor task per thread with pending inbox work; threads run in parallel, never within."""

from __future__ import annotations

import asyncio
import logging

from .actor import Runtime, ThreadActor

logger = logging.getLogger(__name__)
SWEEP_INTERVAL = 30.0


class ThreadManager:
    def __init__(self, runtime: Runtime):
        self.runtime = runtime
        self.actors: dict[str, asyncio.Task] = {}
        self.again: set[str] = set()

    def notify(self, session_id: str) -> None:
        """A thread has new inbox work: start its actor, or make the running one look again."""
        task = self.actors.get(session_id)
        if task is not None and not task.done():
            self.again.add(session_id)
            return
        self.actors[session_id] = asyncio.get_running_loop().create_task(self._drain(session_id), name=f"thread-{session_id}")

    async def _drain(self, session_id: str) -> None:
        try:
            while True:
                self.again.discard(session_id)
                await ThreadActor(self.runtime, session_id).run()
                if session_id not in self.again:
                    return
        except Exception:
            logger.exception("the actor for thread %s failed; its pending work will be retried", session_id)
            try:
                self.runtime.store.inbox.release_session(session_id)
            except Exception:
                logger.exception("could not release thread %s's in-flight items", session_id)
        finally:
            if self.actors.get(session_id) is asyncio.current_task():
                self.actors.pop(session_id, None)

    def sweep(self) -> None:
        """Start actors for every thread with pending work (covers doorbells rung before startup)."""
        for session_id in self.runtime.store.inbox.pending_sessions():
            self.notify(session_id)

    async def run(self) -> None:
        while True:
            self.sweep()
            await asyncio.sleep(SWEEP_INTERVAL)

    async def idle(self) -> None:
        """Wait until no actor is running (tests and shutdown)."""
        while self.actors:
            await asyncio.gather(*list(self.actors.values()), return_exceptions=True)

    async def close(self) -> None:
        tasks = list(self.actors.values())
        for task in tasks:
            task.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)
        self.actors.clear()
