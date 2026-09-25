import asyncio
import tomllib

import pytest
from slack_sdk.errors import SlackApiError

from importlib.resources import files

from fridica.cli.main import main
from fridica.config.discovery import discover, discover_from_config, select_channels


class Client:
    async def auth_test(self):
        return {"user_id": "UOWNER", "team_id": "TTEAM"}

    async def conversations_list(self, **kwargs):
        assert kwargs["exclude_archived"]
        if kwargs["types"] == "private_channel":
            raise SlackApiError("SECRET", {"error": "missing_scope"})
        if not kwargs["cursor"]:
            return {"channels": [{"id": "COTHER", "name": "other", "is_member": False}],
                    "response_metadata": {"next_cursor": "next"}}
        return {"channels": [
            {"id": "CROOM", "name": "general", "is_member": True},
            {"id": "COLD", "name": "old", "is_member": True, "is_archived": True},
        ]}


def test_discover_identity_pagination_and_missing_private_scope():
    owner, workspace, channels, warnings = asyncio.run(discover(Client()))
    assert (owner, workspace) == ("UOWNER", "TTEAM")
    assert channels == [{"id": "CROOM", "name": "general"}]
    assert "groups:read" in warnings[0]


def test_private_channels():
    class PrivateClient(Client):
        async def conversations_list(self, **kwargs):
            if kwargs["types"] == "private_channel":
                return {"channels": [{"id": "GROOM", "name": "private", "is_member": True}]}
            return {"channels": []}

    assert asyncio.run(discover(PrivateClient()))[2] == [{"id": "GROOM", "name": "private"}]


def test_api_error_sanitization():
    class BadClient(Client):
        async def auth_test(self):
            raise SlackApiError("SECRET", {"error": "xoxp-secret"})

    with pytest.raises(ValueError, match="unknown_error") as caught:
        asyncio.run(discover(BadClient()))
    assert "secret" not in str(caught.value).lower()


def test_bot_identity_rejected():
    class BotClient(Client):
        async def auth_test(self):
            return {"user_id": "UBOT", "team_id": "TTEAM", "bot_id": "BBOT"}

    with pytest.raises(ValueError, match="user identity"):
        asyncio.run(discover(BotClient()))


def test_channel_selection(monkeypatch):
    channels = [{"id": "CROOM", "name": "general"}, {"id": "GROOM", "name": "private"}]
    assert select_channels(channels, ["#private", "general", "private"]) == ["GROOM", "CROOM"]
    monkeypatch.setattr("sys.stdin.isatty", lambda: True)
    monkeypatch.setattr("builtins.input", lambda prompt: "2,1")
    assert select_channels(channels, None) == ["GROOM", "CROOM"]
    monkeypatch.setattr("builtins.input", lambda prompt: "")
    with pytest.raises(ValueError, match="cancelled"):
        select_channels(channels, None)
    monkeypatch.setattr("sys.stdin.isatty", lambda: False)
    with pytest.raises(ValueError, match="terminal"):
        select_channels(channels, None)
    with pytest.raises(ValueError, match="not found"):
        select_channels(channels, ["missing"])


TEMPLATE = files("fridica.config").joinpath("template.toml").read_text()


def test_discovery_reads_custom_token_without_valid_full_config(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    path.write_text(TEMPLATE + '\nuser_token_env_extra = "ignored"\n')
    path.write_text(path.read_text().replace('user_token_env = "SLACK_USER_TOKEN"', 'user_token_env = "CUSTOM_USER_TOKEN"'))
    monkeypatch.setenv("CUSTOM_USER_TOKEN", "xoxp-test")

    def client_factory(**kwargs):
        assert kwargs["token"] == "xoxp-test"
        return Client()

    monkeypatch.setattr("fridica.config.discovery.AsyncWebClient", client_factory)
    assert asyncio.run(discover_from_config(path))[0] == "UOWNER"


def test_detect_cli_saves_ids_only_after_selection(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    path.write_text(TEMPLATE)

    async def fake_discover(config_path):
        return "UOWNER", "TTEAM", [{"id": "CROOM", "name": "general"}], []

    monkeypatch.setattr("fridica.config.discovery.discover_from_config", fake_discover)
    assert main(["configure", "--config", str(path), "--detect", "--channel-name", "missing"]) == 2
    assert path.read_text() == TEMPLATE
    assert main(["configure", "--config", str(path), "--detect", "--channel-name", "general"]) == 0
    values = tomllib.loads(path.read_text())
    assert (values["owner"]["slack_user"], values["slack"]["workspace"], values["slack"]["channels"]) == ("UOWNER", "TTEAM", ["CROOM"])


def test_detect_conflicting_options(tmp_path):
    assert main(["configure", "--detect", "--owner-id", "UOWNER"]) == 2
    assert main(["configure", "--channel-name", "general"]) == 2
