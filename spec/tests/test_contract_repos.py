import pytest

from fridica.parent.contract import load, parse
from fridica.parent.repos import REPOS_LIMIT, default_repos_text, load_repos, parse_repos

VALID = '''
[[repos]]
name = "snapy"
url = "https://github.com/chengcli/snapy"
collaborators = ["UJ4L4998Q", "Tianhao"]

[[repos]]
name = "pydisort"
url = "https://github.com/zoeyzyhu/pydisort.git"
collaborators = ["Zoey Hu"]
notes = "radiative transfer solver"
'''


def test_packaged_contract_has_every_section():
    contract = load(None)
    assert "first-person voice" in contract.replies and "Delegate work" in contract.delegation
    assert "WorkerResult" not in contract.workers and "report field" in contract.workers
    assert not hasattr(contract, "summaries") and contract.debriefs and contract.extra.startswith("## Repo rules")
    assert "## Repo rules" in contract.parent and "## Repo rules" in contract.worker
    assert "Delegate work" in contract.parent and "Delegate work" not in contract.worker


def test_owner_contract_needs_the_required_sections_and_inherits_the_rest(tmp_path):
    path = tmp_path / "contract.md"
    path.write_text("# mine\n\n## Participation\n\n- only when asked\n\n## Replies\n\n- be terse\n\n## Lab rules\n\n- no Fridays\n")
    contract = load(path)
    assert contract.participation == "- only when asked" and contract.replies == "- be terse"
    assert contract.delegation == load(None).delegation and contract.extra == "## Lab rules\n\n- no Fridays"
    path.write_text("## Replies\n\n- hi\n")
    with pytest.raises(ValueError, match="Participation"):
        load(path)
    with pytest.raises(ValueError, match="KiB"):
        parse("## Participation\n\nx\n\n## Replies\n\n" + "y" * 70000)


def test_parse_repos_and_payload():
    repos = parse_repos(VALID)
    assert [repo.name for repo in repos] == ["snapy", "pydisort"] and repos[0].owner == "UJ4L4998Q"
    assert repos[1].payload()["notes"] == "radiative transfer solver"
    assert load_repos(None) == parse_repos(default_repos_text())


@pytest.mark.parametrize("text, error", [
    ("[[repos]]\nname = 'snapy'\n", "https URL"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\ncollaborators = ['o']\npath = '~/a'\n", "local paths"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\ncollaborators = []\n", "at least the owner"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\ncollaborators = ['o']\n[[repos]]\nname = 'A'\nurl = 'https://github.com/a/c'\ncollaborators = ['o']\n", "listed twice"),
    ("[[repos]]\nname = 'a'\nurl = 'https://github.com/a/b'\ncollaborators = ['o']\nnotes = '" + "x" * REPOS_LIMIT + "'\n", "KiB"),
])
def test_parse_repos_rejects_bad_entries(text, error):
    with pytest.raises(ValueError, match=error):
        parse_repos(text)


def test_shared_list_is_valid_and_complete():
    repos = parse_repos(default_repos_text())
    assert repos
    for repo in repos:
        assert repo.url.startswith("https://github.com/") and repo.owner
    assert "pull request" in default_repos_text()
