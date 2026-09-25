"""Calls to the Slack Web API as the owner, with delivery errors mapped to outbox outcomes."""

from __future__ import annotations

import logging
import math
import re
from typing import Protocol

from slack_sdk.errors import SlackApiError
from slack_sdk.web.async_client import AsyncWebClient

from ..config.schema import Config
from ..core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from ..core.models import FridicaMeta
from .ingress import TEXT_LIMIT
from .render import metadata

logger = logging.getLogger(__name__)
LINKED_REPLY_LIMIT = 50
PAGES = 10
AMBIGUOUS_ERRORS = {"internal_error", "fatal_error", "request_timeout", "service_unavailable"}


class SlackAPI(Protocol):
    async def post(self, channel: str, text: str, *, thread_ts: str | None, meta: FridicaMeta | None) -> str: ...

    async def upload(self, channel: str, thread_ts: str | None, data: bytes, filename: str) -> str: ...

    async def fetch(self, channel: str, ts: str, thread: str | None) -> list[dict]: ...

    async def recent(self, channel: str, oldest: float, threads: tuple[str, ...] = ()) -> list[dict]: ...


def _failure(error: SlackApiError) -> Exception:
    response = error.response
    if response.status_code == 429:
        header = response.headers.get("Retry-After", response.headers.get("retry-after", "30"))
        if isinstance(header, list):
            header = header[0]
        try:
            delay = float(header)
        except (TypeError, ValueError):
            delay = 30.0
        return RateLimited(delay if math.isfinite(delay) else 30.0)
    code = response.get("error", "unknown_error") if hasattr(response, "get") else "unknown_error"
    if response.status_code >= 500 or code in AMBIGUOUS_ERRORS:
        return DeliveryAmbiguous(code)
    return DeliveryRejected(code)


class SlackClient:
    def __init__(self, config: Config, client: AsyncWebClient):
        self.config = config
        self.client = client

    async def validate(self) -> None:
        """The user token must belong to the configured owner, who must be in every configured channel."""
        response = await self.client.auth_test()
        if (response.get("user_id") != self.config.owner.slack_user or response.get("team_id") != self.config.slack.workspace
                or response.get("bot_id")):
            raise ValueError("the Slack user token does not belong to the configured owner and workspace")
        for channel in self.config.slack.channels:
            info = await self.client.conversations_info(channel=channel)
            if not info["channel"].get("is_member"):
                raise ValueError(f"the owner must be a member of channel {channel}")

    async def post(self, channel: str, text: str, *, thread_ts: str | None, meta: FridicaMeta | None) -> str:
        arguments = {"channel": channel, "text": text, "unfurl_links": False, "unfurl_media": False}
        if meta is not None:
            arguments["metadata"] = metadata(meta)
        if thread_ts is not None:
            arguments["thread_ts"] = thread_ts
        try:
            response = await self.client.chat_postMessage(**arguments)
        except SlackApiError as error:
            raise _failure(error) from None
        except (OSError, TimeoutError) as error:
            raise DeliveryAmbiguous(type(error).__name__) from None
        ts = response.get("ts")
        if not isinstance(ts, str) or not re.fullmatch(r"\d+\.\d+", ts):
            raise DeliveryAmbiguous("Slack did not confirm a message timestamp")
        return ts

    async def upload(self, channel: str, thread_ts: str | None, data: bytes, filename: str) -> str:
        try:
            response = await self.client.files_upload_v2(channel=channel, thread_ts=thread_ts, file=data,
                                                         filename=filename, title=filename)
        except SlackApiError as error:
            raise _failure(error) from None
        except (OSError, TimeoutError) as error:
            raise DeliveryAmbiguous(type(error).__name__) from None
        file = response.get("file") or {}
        return file.get("id", "") if isinstance(file, dict) else ""

    async def fetch(self, channel: str, ts: str, thread: str | None) -> list[dict]:
        """A linked message, or a thread root with its replies (conversations.replies returns from the root)."""
        response = await self.client.conversations_replies(channel=channel, ts=thread or ts, limit=LINKED_REPLY_LIMIT + 1)
        messages = [item for item in response.get("messages") or [] if isinstance(item, dict) and isinstance(item.get("text"), str)]
        for index, item in enumerate(messages):
            if item.get("ts") == ts:
                root = item.get("thread_ts") in (None, ts)
                chosen = messages[index:] if root else [item]
                return [{"sender": entry.get("user") or entry.get("bot_id") or "", "text": entry["text"][:TEXT_LIMIT],
                         "ts": entry.get("ts", "")} for entry in chosen]
        return []

    async def recent(self, channel: str, oldest: float, threads: tuple[str, ...] = ()) -> list[dict]:
        """Messages since ``oldest`` (top level, replies in recent roots, and replies in ``threads``) as payloads."""
        bound = f"{oldest:.6f}"
        found = {item["ts"]: item for item in await self._pages(self.client.conversations_history, channel=channel, oldest=bound)}
        roots = set(threads) | {ts for ts, item in found.items() if item.get("reply_count")}
        for root in sorted(roots):
            for item in await self._pages(self.client.conversations_replies, channel=channel, ts=root, oldest=bound):
                if item["ts"] != root:
                    found.setdefault(item["ts"], item)
        payloads = []
        for ts, item in sorted(found.items()):
            if not re.fullmatch(r"\d+\.\d+", ts) or float(ts) < oldest:
                continue
            payloads.append({"type": "event_callback", "event_id": f"catchup:{channel}:{ts}",
                             "team_id": self.config.slack.workspace, "event": {**item, "type": "message", "channel": channel}})
        return payloads

    @staticmethod
    async def _pages(method, **arguments) -> list[dict]:
        items, cursor = [], None
        for _ in range(PAGES):
            response = await method(**arguments, limit=200, include_all_metadata=True, **({"cursor": cursor} if cursor else {}))
            items += [item for item in response.get("messages") or [] if isinstance(item, dict) and isinstance(item.get("ts"), str)]
            cursor = (response.get("response_metadata") or {}).get("next_cursor")
            if not cursor:
                break
        else:
            logger.warning("catch-up stopped after %d pages; older messages in this window were not read", PAGES)
        return items
