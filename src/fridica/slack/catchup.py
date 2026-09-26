"""Catching up on messages Socket Mode did not deliver.

Socket Mode drops events while the daemon is down or reconnecting, and occasionally
while connected. Each channel keeps a watermark: the time of its last complete pass.
A pass rereads the channel from there (less a small overlap), so a daemon that was
down for hours or days sees everything it missed; it never reaches back more than
seven days. The watermark only advances after a complete pass, so a pass that hit the
paging cap is repeated from the same point. Until a channel has a watermark, the
newest message stored before this daemon started stands in for it; a channel with
nothing stored starts an hour back, so a first start does not pull the whole channel.
While running, passes every five minutes cover at least the last fifteen. Replies are
refetched for threads that were active shortly before the window. Messages already
stored are ignored by the (channel, ts) uniqueness, so overlapping windows are harmless.

Missed messages from the last day are handled as usual, and so are older ones that
mention the owner. Other older messages are stored as the thread's history but not
answered: a burst of replies to week-old chatter after an outage would be noise.
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
"""How far back a channel with nothing stored is read."""
RECENT = 900.0
MAX_WINDOW = 7 * 86400.0
OVERLAP = 60.0
INTERVAL = 300.0
THREAD_AGE = 86400.0
REPLY_AGE = 86400.0
"""Caught-up messages older than this are history only, never work."""


def watermark_key(workspace: str, channel: str) -> str:
    return f"catchup:{workspace}:{channel}"


def oldest(store: Store, workspace: str, channel: str, window: float, now: float,
           started_at: float | None = None) -> float:
    """Where a channel's pass starts: its watermark (or, before the first complete pass, its newest message stored
    before ``started_at``) less OVERLAP when that is further back than ``window``; never more than MAX_WINDOW ago."""
    start = now - window
    mark = store.db.meta(watermark_key(workspace, channel))
    since = float(mark) if mark else store.messages.latest_ts(workspace, channel, received_before=started_at)
    if since is not None:
        start = min(start, since - OVERLAP)
    return max(start, now - MAX_WINDOW)


async def catch_up(slack: SlackAPI, store: Store, config: Config, bus: Bus, window: float, now: float,
                   started_at: float | None = None) -> int:
    """Store missed messages; raises IncompleteHistory (after storing what was read) if any channel was truncated."""
    added, incomplete = 0, None
    owner = f"<@{config.owner.slack_user}>"
    for channel in config.slack.channels:
        start = oldest(store, config.slack.workspace, channel, window, now, started_at)
        threads = tuple(session.key.root_ts for session in store.threads.list(channel=channel, limit=200)
                        if session.updated >= start - THREAD_AGE)
        complete = True
        try:
            payloads = await slack.recent(channel, start, threads)
        except IncompleteHistory as error:
            payloads, incomplete, complete = error.payloads, error, False
        old = 0
        for payload in payloads:
            message = normalize(payload, source="catchup")
            if message is None or message.workspace != config.slack.workspace:
                continue
            work = float(message.ts) >= now - REPLY_AGE or owner in message.text
            session_id, inbox_id = store.messages.intake(message, now, work=work)
            if inbox_id is not None:
                added += 1
                logger.info("caught up on message %s in %s that Slack did not deliver live", message.ts, channel)
                bus.thread(session_id)
            elif not work:
                old += 1
        if old:
            logger.info("stored %d older missed messages in %s as history only", old, channel)
        key = watermark_key(config.slack.workspace, channel)
        if complete:
            store.db.set_meta(key, f"{now:.6f}")
        elif store.db.meta(key) is None:
            # Pin the start: what this pass stored must not stand in for a watermark it never earned.
            store.db.set_meta(key, f"{start + OVERLAP:.6f}")
    if incomplete is not None:
        raise incomplete
    return added


async def run(slack: SlackAPI, store: Store, config_source, bus: Bus, clock: Clock,
              started_at: float | None = None) -> None:
    window = WINDOW
    while True:
        try:
            await catch_up(slack, store, config_source(), bus, window, clock.now(), started_at)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            logger.warning("catching up on missed messages failed (%s)", type(error).__name__)
            window = WINDOW
        else:
            window = RECENT
        await clock.sleep(INTERVAL)
