from __future__ import annotations

import asyncio
import logging
import math
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
    if event.get("subtype") not in (None, "file_share") or event.get("bot_id"):
        return None
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
        try:
            response = await self.client.chat_postMessage(
                channel=message.channel_id, thread_ts=message.thread_id, text=result.text,
                unfurl_links=False, unfurl_media=False,
                metadata={"event_type": "fridica_message", "event_payload": {
                    "owner": self.config.owner_id, "task_id": task_id, "turn": turn, "status": result.status,
                }},
            )
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


async def serve(config: Config, store, agent, observe_only: bool = False) -> None:
    app_token, user_token = config.tokens()
    async with aiohttp.ClientSession() as session:
        web = AsyncWebClient(token=user_token, session=session, retry_handlers=[])
        transport = SlackTransport(config, web)
        await transport.validate()
        replica = Replica(config, store, agent, transport, observe_only)
        socket = SocketModeClient(app_token=app_token, web_client=web)

        async def receive(client, request):
            if request.type == "events_api":
                message = normalize(request.payload)
                if message is not None:
                    replica.receive(message)
            await client.send_socket_mode_response(SocketModeResponse(envelope_id=request.envelope_id))

        socket.socket_mode_request_listeners.append(receive)
        try:
            await socket.connect()
            logger.info("Listening as %s in %d configured channels", config.owner_id, len(config.channels))
            await replica.run()
        finally:
            await socket.close()
