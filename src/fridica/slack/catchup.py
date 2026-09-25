"""Catching up on messages Socket Mode did not deliver.

Socket Mode drops events while the daemon is down or reconnecting, and occasionally
while connected. At start Fridica rereads the last hour of each channel, then every
five minutes the last fifteen; after a failed pass the next one covers the hour
again. Messages already stored are ignored by the (channel, ts) uniqueness.
"""

from __future__ import annotations

import asyncio
import logging

from ..config.schema import Config
from ..core.bus import Bus
from ..core.clock import Clock
from ..store import Store
from .egress import IncompleteHistory, SlackAPI
from .ingress import normalize

logger = logging.getLogger(__name__)
WINDOW = 3600.0
RECENT = 900.0
INTERVAL = 300.0
THREAD_AGE = 86400.0


async def catch_up(slack: SlackAPI, store: Store, config: Config, bus: Bus, window: float, now: float) -> int:
    """Store missed messages; raises IncompleteHistory (after storing what was read) if any channel was truncated."""
    added, incomplete = 0, None
    for channel in config.slack.channels:
        threads = tuple(session.key.root_ts for session in store.threads.list(channel=channel, limit=200)
                        if session.updated >= now - THREAD_AGE)
        try:
            payloads = await slack.recent(channel, now - window, threads)
        except IncompleteHistory as error:
            payloads, incomplete = error.payloads, error
        for payload in payloads:
            message = normalize(payload, source="catchup")
            if message is None or message.workspace != config.slack.workspace:
                continue
            session_id, inbox_id = store.messages.intake(message, now)
            if inbox_id is not None:
                added += 1
                logger.info("caught up on message %s in %s that Slack did not deliver live", message.ts, channel)
                bus.thread(session_id)
    if incomplete is not None:
        raise incomplete
    return added


async def run(slack: SlackAPI, store: Store, config_source, bus: Bus, clock: Clock) -> None:
    window = WINDOW
    while True:
        try:
            await catch_up(slack, store, config_source(), bus, window, clock.now())
        except asyncio.CancelledError:
            raise
        except Exception as error:
            logger.warning("catching up on missed messages failed (%s)", type(error).__name__)
            window = WINDOW
        else:
            window = RECENT
        await clock.sleep(INTERVAL)
