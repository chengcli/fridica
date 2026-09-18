import importlib.util
import io
from pathlib import Path
import sys
import tarfile
import zipfile

import pytest


spec = importlib.util.spec_from_file_location("release", Path(__file__).parents[1] / "scripts/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


@pytest.mark.parametrize("labels, expected", [
    ([], "v2.3.5"), (["bug"], "v2.3.5"),
    (["release:patch"], "v2.3.5"), (["release:minor"], "v2.4.0"),
    (["release:major"], "v3.0.0"),
])
def test_next_tag_bumps(labels, expected):
    assert release.next_tag(["v2.3.4"], labels) == expected


def test_next_tag_first_release_and_numeric_sort():
    assert release.next_tag([], []) == "v0.1.0"
    assert release.next_tag(["legacy", "v2.0.0rc1"], ["release:major"]) == "v0.1.0"
    assert release.next_tag(["v1.9.9", "v1.10.2", "v1.10.11", "v99.0.0rc1"], []) == "v1.10.12"


@pytest.mark.parametrize("tags", [[], ["v1.0.0"]])
def test_conflicting_labels_fail(tags):
    with pytest.raises(ValueError, match="only one"):
        release.next_tag(tags, ["release:major", "release:minor"])


@pytest.mark.parametrize("tag", [
    "1.2.3", "v01.2.3", "v1.02.3", "v1.2.03", "v1.2.3rc1",
    "v1.2.3\n", "v1.2.3; touch injected", "$(whoami)", "--help",
])
def test_parse_tag_rejects_invalid_input(tag):
    with pytest.raises(ValueError, match="release tag"):
        release.parse_tag(tag)


def test_parse_tag_valid():
    assert release.parse_tag("v0.12.345") == (0, 12, 345)


def make_artifacts(directory, wheel_name="fridica", source_name="fridica",
                   wheel_version="1.2.3", source_version="1.2.3",
                   manifest=True, wheel_metadata=True, source_metadata=True):
    wheel = directory / "fridica-1.2.3-py3-none-any.whl"
    source = directory / "fridica-1.2.3.tar.gz"
    with zipfile.ZipFile(wheel, "w") as archive:
        if wheel_metadata:
            archive.writestr("fridica-1.2.3.dist-info/METADATA", f"Name: {wheel_name}\nVersion: {wheel_version}\n")
        if manifest:
            archive.writestr("fridica/manifest.yaml", "display_information: {}\n")
    with tarfile.open(source, "w:gz") as archive:
        if source_metadata:
            metadata = f"Name: {source_name}\nVersion: {source_version}\n".encode()
            member = tarfile.TarInfo("fridica-1.2.3/PKG-INFO")
            member.size = len(metadata)
            archive.addfile(member, io.BytesIO(metadata))
    return wheel, source


def test_verify_artifacts(tmp_path):
    make_artifacts(tmp_path)
    release.verify_artifacts(tmp_path, "v1.2.3")


@pytest.mark.parametrize("changes, error", [
    ({"manifest": False}, "Slack manifest"),
    ({"wheel_metadata": False}, "wheel metadata"),
    ({"source_metadata": False}, "source distribution metadata"),
    ({"wheel_name": "other"}, "name/version"),
    ({"source_name": "other"}, "name/version"),
    ({"wheel_version": "1.2.4"}, "name/version"),
    ({"source_version": "1.2.3.dev1"}, "name/version"),
])
def test_verify_artifacts_rejects_bad_content(tmp_path, changes, error):
    make_artifacts(tmp_path, **changes)
    with pytest.raises(ValueError, match=error):
        release.verify_artifacts(tmp_path, "v1.2.3")


@pytest.mark.parametrize("suffix", ["whl", "tar.gz"])
@pytest.mark.parametrize("duplicate", [False, True])
def test_verify_requires_one_of_each_distribution(tmp_path, suffix, duplicate):
    make_artifacts(tmp_path)
    artifact = next(tmp_path.glob(f"*.{suffix}"))
    if duplicate:
        (tmp_path / f"extra.{suffix}").write_bytes(artifact.read_bytes())
    else:
        artifact.unlink()
    with pytest.raises(ValueError, match="exactly one"):
        release.verify_artifacts(tmp_path, "v1.2.3")


@pytest.mark.parametrize("current, expected", [
    ("", "tag=v1.3.0\ncreate=true\n"),
    ("other\nv1.2.3\n", "tag=v1.2.3\ncreate=false\n"),
])
def test_next_cli_only_reads_git(monkeypatch, capsys, current, expected):
    commands = []

    def check_output(command, *, text):
        commands.append(command)
        assert text is True
        return "v1.2.3\n" if command == ["git", "tag", "--list"] else current

    monkeypatch.setattr(release.subprocess, "check_output", check_output)
    monkeypatch.setattr(sys, "argv", ["release.py", "next"])
    monkeypatch.setenv("LABELS_JSON", '[{"name": "release:minor"}]')
    release.main()
    assert capsys.readouterr().out == expected
    assert commands == [["git", "tag", "--list"], ["git", "tag", "--points-at", "HEAD"]]


def test_next_cli_rejects_ambiguous_commit_tags(monkeypatch):
    monkeypatch.setattr(release.subprocess, "check_output", lambda *args, **kwargs: "v1.0.0\nv2.0.0\n")
    monkeypatch.setattr(sys, "argv", ["release.py", "next"])
    monkeypatch.delenv("LABELS_JSON", raising=False)
    with pytest.raises(ValueError, match="multiple release tags"):
        release.main()
