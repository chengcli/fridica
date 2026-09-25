"""The outbox dispatcher: every Slack post, in order, exactly once or visibly not at all.

Posts are committed to the outbox before anything is sent. The dispatcher sends
due posts in per-thread order; a rate limit reschedules, a rejection fails the
post, and an unknown outcome (5xx, dropped connection) marks it ambiguous, which
is never resent automatically because it may already be in Slack. A successful
post is recorded as a ``self`` message, so thread history includes it and the
Socket Mode echo of it is ignored.
"""

from __future__ import annotations

import logging

from ..core.bus import Bus
from ..core.clock import Clock
from ..core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from ..core.models import Message, OutboxItem
from ..store import Store
from .egress import SlackAPI

logger = logging.getLogger(__name__)
MAX_ATTEMPTS = 5
POLL_INTERVAL = 5.0


class OutboxDispatcher:
    def __init__(self, store: Store, slack: SlackAPI, bus: Bus, *, owner: str, clock: Clock | None = None):
        self.store = store
        self.slack = slack
        self.bus = bus
        self.owner = owner
        self.clock = clock or Clock()

    async def run(self) -> None:
        while True:
            await self.drain()
            await self.bus.outbox.wait(POLL_INTERVAL)

    async def drain(self) -> int:
        """Send every post that is due; returns how many were sent."""
        sent = 0
        while True:
            items = self.store.outbox.ready(self.clock.now())
            if not items:
                return sent
            progressed = False
            for item in items:
                if await self.send(item):
                    sent += 1
                    progressed = True
            if not progressed:
                return sent

    async def send(self, item: OutboxItem) -> bool:
        if not self.store.outbox.claim(item.id):
            return False
        try:
            if item.kind == "upload":
                ts = await self.slack.upload(item.channel, item.thread_ts, item.blob or b"", item.filename or "file")
            else:
                ts = await self.slack.post(item.channel, item.text, thread_ts=item.thread_ts, meta=item.meta)
        except RateLimited as error:
            attempts = item.attempts + 1
            if attempts >= MAX_ATTEMPTS:
                self.store.outbox.fail(item.id, "failed", f"rate limited {attempts} times")
            else:
                self.store.outbox.retry(item.id, self.clock.now() + max(1.0, min(error.retry_after, 3600.0)), str(error))
            return False
        except DeliveryRejected as error:
            logger.error("Slack rejected a %s post for %s: %s", item.kind, item.session_id, error)
            self.store.outbox.fail(item.id, "failed", str(error))
            return False
        except DeliveryAmbiguous as error:
            logger.error("a %s post for %s may or may not have reached Slack (%s); not resending", item.kind,
                         item.session_id, error)
            self.store.outbox.fail(item.id, "ambiguous", str(error))
            return False
        except Exception as error:
            logger.exception("sending a %s post failed unexpectedly", item.kind)
            self.store.outbox.fail(item.id, "ambiguous", type(error).__name__)
            return False
        now = self.clock.now()
        with self.store.transaction():
            self.store.outbox.sent(item.id, ts)
            if item.kind != "upload":
                self.store.messages.intake(Message(
                    event_id=f"self:{item.channel}:{ts}", workspace=item.session_id.split(":", 1)[0], channel=item.channel,
                    ts=ts, thread_ts=item.thread_ts, sender=self.owner, text=item.text, source="self", meta=item.meta), now)
        return True
