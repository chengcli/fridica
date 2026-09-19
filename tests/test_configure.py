import stat
import tomllib

import pytest

from fridica.cli import main
from fridica.config import TEMPLATE, set_slack_ids


def test_configure_template_preserves_other_settings(tmp_path, capsys):
    path = tmp_path / "config.toml"
    path.write_text(TEMPLATE)
    assert main(["configure", "--config", str(path), "--owner-id", "UOWNER",
                 "--workspace-id", "TTEAM", "--channel-id", "CROOM",
                 "--channel-id", "GPRIVATE", "--channel-id", "CROOM"]) == 0
    data = tomllib.loads(path.read_text())
    assert data["owner_id"] == "UOWNER"
    assert data["workspace_id"] == "TTEAM"
    assert data["channels"] == ["CROOM", "GPRIVATE"]
    assert data["workspace"] == "~/projects/your-project"
    assert '# model = "your-preferred-model"' in path.read_text()
    assert stat.S_IMODE(path.stat().st_mode) == 0o600
    assert "separate state_path" in capsys.readouterr().out


def test_partial_update_and_replace_channels(tmp_path):
    path = tmp_path / "config.toml"
    path.write_text('owner_id = "UOWNER" # keep comment\nchannels = [\n "COLD",\n]\n')
    set_slack_ids(path, channels=["CNEW"])
    assert tomllib.loads(path.read_text()) == {"owner_id": "UOWNER", "channels": ["CNEW"]}
    assert "# keep comment" in path.read_text()


@pytest.mark.parametrize("updates", [
    {}, {"owner_id": "xoxp-secret"}, {"workspace_id": "CROOM"},
    {"channels": []}, {"channels": ["DROOM"]}, {"owner_id": "<@UOWNER>"},
])
def test_invalid_updates_leave_file_unchanged(tmp_path, updates):
    path = tmp_path / "config.toml"
    path.write_text(TEMPLATE)
    with pytest.raises(ValueError):
        set_slack_ids(path, **updates)
    assert path.read_text() == TEMPLATE


def test_missing_and_malformed_config(tmp_path):
    path = tmp_path / "config.toml"
    with pytest.raises(ValueError, match="fridica init"):
        set_slack_ids(path, owner_id="UOWNER")
    path.write_text('owner_id = "unfinished')
    with pytest.raises(ValueError, match="valid TOML"):
        set_slack_ids(path, owner_id="UOWNER")
    assert path.read_text() == 'owner_id = "unfinished'


def test_reject_symlink(tmp_path):
    target = tmp_path / "real.toml"
    target.write_text(TEMPLATE)
    path = tmp_path / "config.toml"
    path.symlink_to(target)
    with pytest.raises(ValueError, match="symlink"):
        set_slack_ids(path, owner_id="UOWNER")
    assert target.read_text() == TEMPLATE


def test_atomic_write_failure_preserves_original(tmp_path, monkeypatch):
    path = tmp_path / "config.toml"
    path.write_text(TEMPLATE)

    def fail_replace(source, destination):
        raise OSError("test failure")

    monkeypatch.setattr("fridica.config.os.replace", fail_replace)
    with pytest.raises(OSError):
        set_slack_ids(path, owner_id="UOWNER")
    assert path.read_text() == TEMPLATE
    assert list(tmp_path.iterdir()) == [path]


def test_no_options_cli_error(tmp_path, capsys):
    assert main(["configure", "--config", str(tmp_path / "config.toml")]) == 1
    assert "provide --owner-id" in capsys.readouterr().err
