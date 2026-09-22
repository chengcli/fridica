from dataclasses import replace
from pathlib import Path

import pytest

from fridica.agents import ClaudeBackend, _prompt
from fridica.cli import main
from fridica.config import load_config
from fridica.contract import load_contract
from fridica.models import ConversationContext
from fridica.repos import REPOS_LIMIT, Repo, default_repos_text, load_repos, parse_repos

VALID = '''
[[repos]]
name = "snapy"
url = "https://github.com/chengcli/snapy"
collaborators = ["UJ4L4998Q", "Tianhao"]
path = "~/scix/repos/snapy"

[[repos]]
name = "pydisort"
url = "https://github.com/zoeyzyhu/pydisort.git"
notes = "radiative transfer solver"
'''


def test_parse_repos_and_payload():
    repos = parse_repos(VALID)
    assert [r.name for r in repos] == ["snapy", "pydisort"]
    assert repos[0].collaborators == ("UJ4L4998Q", "Tianhao") and repos[0].path == "~/scix/repos/snapy"
    assert repos[0].payload() == {"name": "snapy", "url": "https://github.com/chengcli/snapy",
                                  "collaborators": ["UJ4L4998Q", "Tianhao"], "path": "~/scix/repos/snapy"}
    assert repos[1].payload() == {"name": "pydisort",
                                  "url": "https://github.com/zoeyzyhu/pydisort.git", "notes": "radiative transfer solver"}
    assert parse_repos("") == () and load_repos(None) == ()
    default = parse_repos(default_repos_text())
    assert len(default) == 1 and default[0].name == "example-project" and default[0].path == "~/projects/example-project"


@pytest.mark.parametrize("text,error", [
    ("[[repos]]\nname = 'snapy'\n", "https URL"),
    ("[[repos]]\nurl = 'https://github.com/a/b'\n", "needs a name"),
    ("[[repos]]\nname = 'a'\nurl = 'http://github.com/a/b'\n", "https URL"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\nowner = 'x'\n", "unknown fields"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\ncollaborators = 'bob'\n", "collaborators must be a list"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\n[[repos]]\nname = 'A'\nurl = 'https://github.com/a/c'\n", "listed twice"),
    ("other = 1\n", "must contain only"),
    ("this is not toml = = =\n", "not valid TOML"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\nnotes = '" + "x" * REPOS_LIMIT + "'\n", "KiB"),
])
def test_parse_repos_rejects_bad_entries(text, error):
    with pytest.raises(ValueError, match=error):
        parse_repos(text)


def test_config_discovers_repos_beside_config(config, tmp_path):
    source = tmp_path / "config.toml"
    source.write_text(f'owner_id="UOWNER"\nworkspace_id="TTEAM"\nchannels=["CROOM"]\nworkspace="{config.workspace}"\nstate_path="{config.state_path}"\n')
    assert load_config(source).repos is None
    beside = tmp_path / "repos.toml"
    beside.write_text(VALID)
    assert load_config(source).repos == beside
    custom = tmp_path / "lists" / "mine.toml"
    custom.parent.mkdir()
    custom.write_text(VALID)
    with source.open("a") as stream:
        stream.write('repos = "lists/mine.toml"\n')
    assert load_config(source).repos == custom.resolve()
    source.write_text(source.read_text().replace('repos = "lists/mine.toml"', 'repos = "missing.toml"'))
    with pytest.raises(ValueError, match="repos"):
        load_config(source)


def test_prompt_carries_repositories_as_data(config, message, tmp_path):
    repos = parse_repos(VALID)
    context = ConversationContext([], config.owner_id, "profile", "task", 1)
    prompt = _prompt(message(), context, False, load_contract(None), repos)
    data = prompt.split("Conversation data:\n", 1)[1]
    assert '"repositories": [{"name": "snapy"' in data and '"notes": "radiative transfer solver"' in data
    assert "Resolve which repository a request means" in prompt.split("Conversation data:")[0]
    assert "do not guess: ask with status waiting" in prompt
    assert '"repositories": []' in _prompt(message(), context, True, load_contract(None))
    path = tmp_path / "repos.toml"
    path.write_text(VALID)
    backend = ClaudeBackend(replace(config, repos=path))
    assert [r.name for r in backend.repositories()] == ["snapy", "pydisort"]
    path.write_text(VALID + "\n[[repos]]\nname = 'kintera'\nurl = 'https://github.com/chengcli/kintera'\n")
    assert [r.name for r in backend.repositories()][-1] == "kintera"


def test_init_copies_repos_template(tmp_path, capsys):
    target = tmp_path / "cfg" / "config.toml"
    assert main(["init", "--config", str(target)]) == 0
    repos = target.parent / "repos.toml"
    assert repos.read_text() == default_repos_text()
    assert "repos.toml" in capsys.readouterr().out
    repos.write_text(VALID)
    target.unlink()
    (target.parent / "contract.md").unlink(missing_ok=True)
    (target.parent / "manifest.yaml").unlink(missing_ok=True)
    assert main(["init", "--config", str(target)]) == 0
    assert repos.read_text() == VALID
