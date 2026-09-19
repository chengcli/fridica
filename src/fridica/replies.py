from __future__ import annotations

import re

from .models import ConversationContext, Message


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
