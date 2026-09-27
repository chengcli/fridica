"""Following Slack message links in a thread, so a request that points elsewhere can be read."""

from __future__ import annotations

import logging

from ..core.models import Message
from .egress import SlackAPI
from .render import permalinks

logger = logging.getLogger(__name__)
TEXT_BUDGET = 20000


async def linked(slack: SlackAPI, channels: tuple[str, ...], message: Message, history: list[Message]) -> tuple[dict, ...]:
    """The messages links point to: links in ``message`` first, then in history (newest first).

    Only links into configured channels are followed: anyone in those channels can
    trigger the agent, so following links elsewhere would expose channels they
    cannot read. Links into the current thread are skipped; history has them.
    """
    found, budget = [], TEXT_BUDGET
    for link, channel, ts, root in permalinks("\n".join(item.text for item in [message, *reversed(history)])):
        if budget <= 0:
            break
        if channel not in channels:
            found.append({"link": link, "error": "not in a channel I watch"})
            continue
        if channel == message.channel and (root or ts) == message.root_ts:
            continue
        try:
            entries = await slack.fetch(channel, ts, root)
        except Exception as error:
            logger.warning("linked message %s in %s could not be fetched (%s)", ts, channel, type(error).__name__)
            found.append({"link": link, "error": "could not be fetched"})
            continue
        if not entries:
            found.append({"link": link, "error": "not found"})
        for entry in entries:
            text = entry["text"][:budget]
            budget -= len(text)
            found.append({"link": link, "sender": entry["sender"], "text": text})
            if budget <= 0:
                break
    return tuple(found)
