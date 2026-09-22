from __future__ import annotations

import asyncio
import logging
import math
import time
import re

import aiohttp
from slack_sdk.errors import SlackApiError
from slack_sdk.socket_mode.aiohttp import SocketModeClient
from slack_sdk.socket_mode.response import SocketModeResponse
from slack_sdk.web.async_client import AsyncWebClient

from .config import Config
from .models import AgentResult, Message
from .replica import DeliveryRejected, RateLimited, Replica


logger = logging.getLogger(__name__)
MARKER = "\n\n[via fridica]"


def normalize(payload: dict) -> Message | None:
    if not isinstance(payload, dict) or payload.get("type") != "event_callback":
        return None
    event = payload.get("event")
    if not isinstance(event, dict) or event.get("type") != "message":
        return None
    if event.get("subtype") not in (None, "file_share"):
        return None
    # A message posted through a user token by an app that also has a bot user carries both
    # ``user`` and ``bot_id`` (this is how other owners' Fridica replies arrive). Such messages
    # belong to that user; only messages without a ``user`` are dropped as bot posts.
    fields = [payload.get("event_id"), payload.get("team_id"), event.get("channel"), event.get("user"), event.get("text"), event.get("ts")]
    if any(not isinstance(value, str) or not value for value in fields):
        return None
    timestamp = event["ts"]
    thread = event.get("thread_ts", timestamp)
    if not isinstance(thread, str) or not re.fullmatch(r"\d+\.\d+", thread) or not re.fullmatch(r"\d+\.\d+", timestamp):
        return None
    if not math.isfinite(float(timestamp)):
        return None
    metadata = event.get("metadata", {})
    if not isinstance(metadata, dict):
        metadata = {}
    generated = metadata.get("event_type") == "fridica_message" or event["text"].endswith(MARKER)
    data = metadata.get("event_payload", {}) if generated else {}
    if not isinstance(data, dict):
        data = {}
    turn = data.get("turn", 0)
    if isinstance(turn, bool) or not isinstance(turn, int) or turn < 0:
        turn = 0
    task_id = data.get("task_id")
    return Message(
        event_id=payload["event_id"], workspace_id=payload["team_id"], channel_id=event["channel"],
        sender_id=event["user"], text=event["text"][:40000], timestamp=timestamp, thread_id=thread,
        generated=generated, task_id=task_id if isinstance(task_id, str) and 0 < len(task_id) <= 128 else None, turn=min(turn, 10000),
        task_status=data.get("status") if data.get("status") in ("complete", "waiting", "blocked") else None,
    )


def dropped_mention(payload: object, owner_id: str) -> str | None:
    """Describe an event that ``normalize`` rejected although it @mentions the owner, else None.

    Such drops are otherwise invisible: the event never reaches the database, so a
    missing reply cannot be diagnosed from local state. The description names the
    fields that commonly cause a rejection without quoting the message text.
    """
    if not isinstance(payload, dict):
        return None
    event = payload.get("event")
    if not isinstance(event, dict) or event.get("type") != "message":
        return None
    text = event.get("text")
    if not isinstance(text, str) or f"<@{owner_id}>" not in text:
        return None
    return (f"event {payload.get('event_id')} subtype={event.get('subtype')} user={'set' if event.get('user') else 'missing'} "
            f"bot_id={'set' if event.get('bot_id') else 'none'} ts={event.get('ts')}")


class SlackTransport:
    def __init__(self, config: Config, client: AsyncWebClient):
        self.config = config
        self.client = client

    async def validate(self) -> None:
        response = await self.client.auth_test()
        if response.get("user_id") != self.config.owner_id or response.get("team_id") != self.config.workspace_id or response.get("bot_id"):
            raise ValueError("Slack token identity does not match configured owner and workspace")
        for channel in self.config.channels:
            info = await self.client.conversations_info(channel=channel)
            if not info["channel"].get("is_member"):
                raise ValueError("owner must belong to every configured Slack channel")

    async def send(self, message: Message, result: AgentResult, task_id: str, turn: int) -> str:
        return await self._post(message.channel_id, result.text, task_id, turn, result.status, thread_ts=message.thread_id)

    async def announce(self, message: Message, text: str, task_id: str) -> str:
        """Post a new top-level message in the channel (the root of a continuation thread)."""
        return await self._post(message.channel_id, text, task_id, 0, "complete", thread_ts=None)

    async def _post(self, channel: str, text: str, task_id: str, turn: int, status: str, *, thread_ts: str | None) -> str:
        try:
            arguments = {"channel": channel, "text": text, "unfurl_links": False, "unfurl_media": False,
                         "metadata": {"event_type": "fridica_message", "event_payload": {
                             "owner": self.config.owner_id, "task_id": task_id, "turn": turn, "status": status}}}
            if thread_ts is not None:
                arguments["thread_ts"] = thread_ts
            response = await self.client.chat_postMessage(**arguments)
        except SlackApiError as error:
            if error.response.status_code == 429:
                header = error.response.headers.get("Retry-After", error.response.headers.get("retry-after", "30"))
                if isinstance(header, list):
                    header = header[0]
                try:
                    delay = float(header)
                    if not math.isfinite(delay):
                        delay = 30
                except (ValueError, TypeError):
                    delay = 30
                raise RateLimited(delay) from None
            if error.response.status_code >= 500 or error.response.get("error") in {"internal_error", "fatal_error", "request_timeout"}:
                raise RuntimeError("Slack delivery outcome is unknown") from None
            raise DeliveryRejected(error.response.get("error", "unknown_error")) from None
        timestamp = response.get("ts")
        if not isinstance(timestamp, str) or not re.fullmatch(r"\d+\.\d+", timestamp):
            raise RuntimeError("Slack did not confirm a message timestamp")
        return timestamp


async def serve(config: Config, store, agent, observe_only: bool = False, config_path=None) -> None:
    started_at = time.time()
    store.heartbeat('connecting', observe_only, started_at)
    app_token, user_token = config.tokens()
    async with aiohttp.ClientSession() as session:
        web = AsyncWebClient(token=user_token, session=session, retry_handlers=[])
        transport = SlackTransport(config, web)
        await transport.validate()
        replica = Replica(config, store, agent, transport, observe_only, config_path=config_path)
        socket = SocketModeClient(app_token=app_token, web_client=web)

        async def receive(client, request):
            if request.type == "events_api":
                message = normalize(request.payload)
                if message is not None:
                    replica.receive(message)
                else:
                    reason = dropped_mention(request.payload, config.owner_id)
                    if reason:
                        logger.warning("Ignored a message that mentions you: %s", reason)
            await client.send_socket_mode_response(SocketModeResponse(envelope_id=request.envelope_id))

        async def heartbeat():
            while True:
                connected = await socket.is_connected()
                store.heartbeat('connected' if connected else 'reconnecting', observe_only, started_at)
                await asyncio.sleep(5)

        socket.socket_mode_request_listeners.append(receive)
        try:
            await socket.connect()
            logger.info("Listening as %s in %d configured channels", config.owner_id, len(config.channels))
            async with asyncio.TaskGroup() as workers:
                workers.create_task(heartbeat())
                workers.create_task(replica.run())
        finally:
            await socket.close()
            store.heartbeat('stopped', observe_only, started_at)
