from __future__ import annotations

import asyncio
import os
from pathlib import Path
import re
import sys
import tomllib

import aiohttp
from slack_sdk.errors import SlackApiError
from slack_sdk.web.async_client import AsyncWebClient


async def discover(client) -> tuple[str, str, list[dict], list[str]]:
    try:
        identity = await client.auth_test()
        owner = identity.get("user_id", "")
        workspace = identity.get("team_id", "")
        if identity.get("bot_id") or not re.fullmatch(r"[UW][A-Z0-9]+", owner) or not re.fullmatch(r"T[A-Z0-9]+", workspace):
            raise ValueError("Slack did not return a workspace user identity; use a user token")
        channels = {}
        warnings = []
        for kind, scope in (("public_channel", "channels:read"), ("private_channel", "groups:read")):
            cursor = ""
            seen = set()
            while True:
                try:
                    response = await client.conversations_list(
                        types=kind, exclude_archived=True, limit=200, cursor=cursor,
                    )
                except SlackApiError as error:
                    if error.response.get("error") == "missing_scope":
                        warnings.append(f"Cannot discover {kind.replace('_', ' ')}s: add {scope} as a user scope and reinstall.")
                        break
                    raise
                for channel in response.get("channels", []):
                    if (channel.get("is_member") and not channel.get("is_archived")
                            and not channel.get("is_im") and not channel.get("is_mpim")
                            and re.fullmatch(r"[CG][A-Z0-9]+", channel.get("id", ""))
                            and isinstance(channel.get("name"), str)):
                        channels[channel["id"]] = {"id": channel["id"], "name": channel["name"]}
                cursor = response.get("response_metadata", {}).get("next_cursor", "").strip()
                if not cursor:
                    break
                if cursor in seen:
                    raise ValueError("Slack repeated a pagination cursor; retry discovery")
                seen.add(cursor)
        return owner, workspace, sorted(channels.values(), key=lambda channel: (channel["name"], channel["id"])), warnings
    except SlackApiError as error:
        code = error.response.get("error", "unknown_error")
        if not isinstance(code, str) or not re.fullmatch(r"[a-z][a-z0-9_]{0,63}", code):
            code = "unknown_error"
        raise ValueError(f"Slack discovery failed ({code}); check the user token, scopes, or retry later") from None


async def discover_from_config(path: Path) -> tuple[str, str, list[dict], list[str]]:
    with path.expanduser().open("rb") as stream:
        values = tomllib.load(stream)
    variable = values.get("user_token_env", "FRIDICA_SLACK_USER_TOKEN")
    if not isinstance(variable, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", variable):
        raise ValueError("user_token_env must name an environment variable")
    token = os.environ.get(variable, "")
    if not token.startswith("xoxp-"):
        raise ValueError(f"set {variable} to your Slack user token before detection")
    async with aiohttp.ClientSession() as session:
        client = AsyncWebClient(token=token, session=session, timeout=15, retry_handlers=[])
        try:
            return await asyncio.wait_for(discover(client), timeout=60)
        except (TimeoutError, aiohttp.ClientError):
            raise ValueError("Slack discovery could not connect or timed out; check connectivity and retry") from None


def select_channels(channels: list[dict], names: list[str] | None) -> list[str]:
    if not channels:
        raise ValueError("No joined channels found; check membership and channel read scopes")
    if names is not None:
        selected = []
        for name in names:
            matches = [channel["id"] for channel in channels if channel["name"] == name.removeprefix("#")]
            if len(matches) != 1:
                raise ValueError("A requested channel name was not found or is ambiguous; use interactive selection")
            selected.extend(matches)
        return list(dict.fromkeys(selected))
    if not sys.stdin.isatty():
        raise ValueError("Channel selection needs a terminal; use --channel-name NAME (repeat for multiple channels)")
    for index, channel in enumerate(channels, 1):
        label = "".join(character for character in channel["name"] if character.isprintable())
        print(f"  {index}. #{label} ({channel['id']})")
    try:
        answer = input("Select channel numbers separated by commas (blank cancels): ").strip()
    except EOFError:
        raise ValueError("Selection cancelled; configuration unchanged") from None
    if not answer:
        raise ValueError("Selection cancelled; configuration unchanged")
    numbers = [part.strip() for part in answer.split(",")]
    if any(not number.isdecimal() or not 1 <= int(number) <= len(channels) for number in numbers):
        raise ValueError("Invalid channel selection; configuration unchanged")
    return list(dict.fromkeys(channels[int(number) - 1]["id"] for number in numbers))
