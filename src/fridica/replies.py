from __future__ import annotations

import re

from .models import ConversationContext, Message

# A Slack message permalink: https://<team>.slack.com/archives/<channel>/p<ts without the dot>[?thread_ts=<root>&...]
PERMALINK = re.compile(r"https://[A-Za-z0-9.-]+\.slack\.com/archives/([CG][A-Z0-9]{2,})/p(\d+)(\d{6})(?:\?([^\s|>]*))?")
LINK_LIMIT = 3


def permalinks(text: str) -> list[tuple[str, str, str, str | None]]:
    """The distinct Slack message links in ``text`` as (link, channel, timestamp, thread root or None), at most LINK_LIMIT."""
    found = []
    for match in PERMALINK.finditer(text):
        channel, timestamp = match.group(1), f"{match.group(2)}.{match.group(3)}"
        root = re.search(r"(?:^|&)thread_ts=(\d+\.\d+)", match.group(4) or "")
        entry = (match.group(0).split("?")[0], channel, timestamp, root.group(1) if root else None)
        if all(item[1:3] != entry[1:3] for item in found):
            found.append(entry)
    return found[:LINK_LIMIT]


def format_reply(text: str, message: Message, context: ConversationContext) -> str:
    text = re.sub(r"(?:\s*\[via fridica\])+\s*$", "", text).rstrip()
    participants = {context.owner_id, message.sender_id}
    for entry in [*context.messages, message]:
        participants.add(entry.sender_id)
        participants.update(re.findall(r"<@([UW][A-Z0-9]+)>", entry.text))
    protected = re.compile(r"(```[\s\S]*?```|`[^`]*`|<[^>]*>|https?://[^\s<>]+)")
    identifier = re.compile(r"(?<![\w@])([UW][A-Z0-9]+)(?!\w)")
    pieces = protected.split(text)
    for index in range(0, len(pieces), 2):
        pieces[index] = identifier.sub(
            lambda match: f"<@{match[0]}>" if match[0] in participants else match[0], pieces[index]
        )
    return "".join(pieces)
