"""Calls to the Slack Web API as the owner, with delivery errors mapped to outbox outcomes."""

from __future__ import annotations

import logging
import math
import re
import time
from typing import Protocol

from slack_sdk.errors import SlackApiError
from slack_sdk.web.async_client import AsyncWebClient

from ..config.schema import Config
from ..core.errors import DeliveryAmbiguous, DeliveryRejected, RateLimited
from ..core.models import FridicaMeta
from .ingress import TEXT_LIMIT, file_url
from .render import metadata

logger = logging.getLogger(__name__)
LINKED_REPLY_LIMIT = 50
PAGES = 10
DOWNLOAD_CACHE = 32
FAILURE_TTL = 300.0
AMBIGUOUS_ERRORS = {"internal_error", "fatal_error", "request_timeout", "service_unavailable"}


class FileUnavailable(RuntimeError):
    """An attachment that cannot be read; the message is safe to show the parent."""


class IncompleteHistory(RuntimeError):
    """Paging stopped at the cap: ``payloads`` holds what was read; older messages in the window were not."""

    def __init__(self, payloads: list[dict]):
        super().__init__(f"history paging stopped after {PAGES} pages")
        self.payloads = payloads


class SlackAPI(Protocol):
    async def post(self, channel: str, text: str, *, thread_ts: str | None, meta: FridicaMeta | None) -> str: ...

    async def upload(self, channel: str, thread_ts: str | None, data: bytes, filename: str) -> str: ...

    async def fetch(self, channel: str, ts: str, thread: str | None) -> list[dict]: ...

    async def recent(self, channel: str, oldest: float, threads: tuple[str, ...] = ()) -> list[dict]: ...

    async def user_name(self, user_id: str) -> str: ...

    async def download(self, url: str, limit: int, *, html: bool = False) -> tuple[bytes, int]: ...


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
        self.names: dict[str, str] = {}
        self.scopes: frozenset[str] | None = None
        """The user token's OAuth scopes, read by validate(); None until then."""
        self.downloads: dict[str, tuple[bytes, int]] = {}
        self.failures: dict[str, tuple[float, str]] = {}

    async def validate(self) -> None:
        """The user token must belong to the configured owner, who must be in every configured channel."""
        response = await self.client.auth_test()
        header = (getattr(response, "headers", None) or {}).get("x-oauth-scopes")
        # No header means unknown, not "no scopes": downloads are then attempted and fail on their own.
        self.scopes = None if header is None else frozenset(item.strip() for item in header.split(",") if item.strip())
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
        file = response.get("file") if hasattr(response, "get") else None
        if not isinstance(file, dict) or not isinstance(file.get("id"), str) or not file["id"]:
            files = response.get("files") if hasattr(response, "get") else None
            file = files[0] if isinstance(files, list) and files and isinstance(files[0], dict) else {}
        if not isinstance(file.get("id"), str) or not file["id"]:
            raise DeliveryAmbiguous("Slack did not confirm the uploaded file")
        return file["id"]

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

    async def download(self, url: str, limit: int, *, html: bool = False) -> tuple[bytes, int]:
        """Up to ``limit`` + 1 bytes of a file on files.slack.com, and its full size (0 when unknown).

        Needs the files:read scope. Only Slack's own file host ever receives the token, redirects are not
        followed, and an HTML answer is taken for Slack's sign-in page unless the file itself is HTML (``html``).
        Failures are remembered for FAILURE_TTL seconds.
        """
        import aiohttp

        if self.scopes is not None and "files:read" not in self.scopes:
            raise FileUnavailable("the Slack token lacks files:read")
        if not file_url(url):
            raise FileUnavailable("not a Slack file URL")
        if url in self.downloads:
            return self.downloads[url]
        failed = self.failures.get(url)
        if failed is not None and failed[0] > time.monotonic():
            raise FileUnavailable(failed[1])
        session = getattr(self.client, "session", None)
        owned = session is None
        session = session or aiohttp.ClientSession()
        try:
            async with session.get(url, headers={"Authorization": f"Bearer {self.client.token}"}, allow_redirects=False,
                                   timeout=aiohttp.ClientTimeout(total=20)) as response:
                if response.status != 200 or (response.content_type == "text/html" and not html):
                    # Without files:read Slack answers with its sign-in page instead of the file.
                    raise FileUnavailable("Slack did not return the file")
                data = bytearray()
                while len(data) <= limit:  # read() returns what is buffered, not the whole request
                    chunk = await response.content.read(limit + 1 - len(data))
                    if not chunk:
                        break
                    data += chunk
                size = response.content_length or 0
        except FileUnavailable as error:
            self.failures[url] = (time.monotonic() + FAILURE_TTL, str(error))
            raise
        except (aiohttp.ClientError, TimeoutError) as error:
            message = f"download failed ({type(error).__name__})"
            self.failures[url] = (time.monotonic() + FAILURE_TTL, message)
            raise FileUnavailable(message) from None
        finally:
            if owned:
                await session.close()
        if len(self.downloads) >= DOWNLOAD_CACHE:
            self.downloads.pop(next(iter(self.downloads)))
        self.downloads[url] = (bytes(data), size)
        return self.downloads[url]

    async def user_name(self, user_id: str) -> str:
        """A member's display name for plain-text use (no mention); the ID itself when it cannot be looked up."""
        cache = self.names
        if user_id not in cache:
            try:
                response = await self.client.users_info(user=user_id)
                user = response.get("user") or {}
                profile = user.get("profile") or {}
                cache[user_id] = (profile.get("display_name") or profile.get("real_name") or user.get("real_name")
                                  or user.get("name") or user_id)
            except Exception as error:
                logger.warning("could not look up the name of %s (%s)", user_id, type(error).__name__)
                return user_id
        return cache[user_id]

    async def recent(self, channel: str, oldest: float, threads: tuple[str, ...] = ()) -> list[dict]:
        """Messages since ``oldest`` (top level, replies in recent roots, and replies in ``threads``) as payloads.

        Raises IncompleteHistory, carrying what was read, when paging hit its cap.
        """
        bound = f"{oldest:.6f}"
        items, complete = await self._pages(self.client.conversations_history, channel=channel, oldest=bound)
        found = {item["ts"]: item for item in items}
        roots = set(threads) | {ts for ts, item in found.items() if item.get("reply_count")}
        for root in sorted(roots):
            replies, whole = await self._pages(self.client.conversations_replies, channel=channel, ts=root, oldest=bound)
            complete = complete and whole
            for item in replies:
                if item["ts"] != root:
                    found.setdefault(item["ts"], item)
        payloads = []
        for ts, item in sorted(found.items()):
            if not re.fullmatch(r"\d+\.\d+", ts) or float(ts) < oldest:
                continue
            payloads.append({"type": "event_callback", "event_id": f"catchup:{channel}:{ts}",
                             "team_id": self.config.slack.workspace, "event": {**item, "type": "message", "channel": channel}})
        if not complete:
            raise IncompleteHistory(payloads)
        return payloads

    @staticmethod
    async def _pages(method, **arguments) -> tuple[list[dict], bool]:
        """Every message of a paginated call up to PAGES pages, and whether the listing was complete."""
        items, cursor = [], None
        for _ in range(PAGES):
            response = await method(**arguments, limit=200, include_all_metadata=True, **({"cursor": cursor} if cursor else {}))
            items += [item for item in response.get("messages") or [] if isinstance(item, dict) and isinstance(item.get("ts"), str)]
            cursor = (response.get("response_metadata") or {}).get("next_cursor")
            if not cursor:
                return items, True
        logger.warning("catch-up stopped after %d pages; the next pass rereads the channel from its watermark", PAGES)
        return items, False
