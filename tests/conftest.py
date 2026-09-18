from dataclasses import replace

import pytest

from fridica.config import Config
from fridica.models import Message
from fridica.store import Store


@pytest.fixture
def config(tmp_path):
    workspace = tmp_path / "project"
    workspace.mkdir()
    return Config("UOWNER", "TTEAM", ("CROOM",), workspace, state_path=tmp_path / "state.sqlite3")


@pytest.fixture
def store(config):
    database = Store(config.state_path)
    yield database
    database.close()


@pytest.fixture
def message():
    def make(event_id="event1", **changes):
        base = Message(event_id, "TTEAM", "CROOM", "UALICE", "<@UOWNER> help", "100.000001", "100.000001")
        return replace(base, **changes)
    return make
