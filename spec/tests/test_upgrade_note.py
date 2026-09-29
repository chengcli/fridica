"""F6: the v0.2 → v0.3 upgrade note names only settings that exist, and its v0.3 example loads."""

from dataclasses import fields
from pathlib import Path
import re
import tomllib

from fridica.config.loader import parse
from fridica.config.schema import GitHubConfig, Limits, OwnerConfig, ParentConfig, SlackConfig, StateConfig
from fridica.machines.registry import POLICY_FIELDS

NOTE = Path(__file__).resolve().parents[1] / "docs" / "upgrade-v0.2.md"
SECTIONS = {"owner": OwnerConfig, "slack": SlackConfig, "parent": ParentConfig, "limits": Limits,
            "state": StateConfig, "github": GitHubConfig}


def test_every_v03_setting_named_in_the_key_map_exists():
    text = NOTE.read_text()
    named = re.findall(r"`\[(owner|slack|parent|limits|state|policy|github)\] ([a-z_]+)`", text)
    assert len(named) >= 20
    for section, key in named:
        allowed = POLICY_FIELDS if section == "policy" else {field.name for field in fields(SECTIONS[section])}
        assert key in allowed, f"[{section}] {key}"


def test_the_v03_example_loads(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    for folder in ("projects/kintera", "data"):
        (tmp_path / folder).mkdir(parents=True)
    example = re.findall(r"```toml\n# v0.3\n(.*?)```", NOTE.read_text(), re.DOTALL)[0]
    config = parse(tomllib.loads(example), base=tmp_path)
    assert [machine.name for machine in config.machines.machines] == ["local", "dart9"]
    assert config.machines.get("local").workspace("data").policy.mode == "read-only"
    assert config.machines.get("dart9").resources.gpus == (0, 1)
    assert config.policy.network == ("*",)  # v0.2's default network access is carried over
