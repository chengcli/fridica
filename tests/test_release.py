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


PLATFORM_TAGS = ["py3-none-manylinux_2_28_x86_64", "py3-none-manylinux_2_28_aarch64",
                 "py3-none-macosx_10_13_x86_64", "py3-none-macosx_11_0_arm64"]


def make_wheel(directory, tag, name="fridica", version="1.2.3", scripts=("fridica",),
               metadata=True):
    wheel = directory / f"fridica-1.2.3-{tag}.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        if metadata:
            archive.writestr("fridica-1.2.3.dist-info/METADATA", f"Name: {name}\nVersion: {version}\n")
            archive.writestr("fridica-1.2.3.dist-info/WHEEL", f"Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: {tag}\n")
        for script in scripts:
            archive.writestr(f"fridica-1.2.3.data/scripts/{script}", "binary")
    return wheel


def make_artifacts(directory, tags=PLATFORM_TAGS, source_name="fridica", source_version="1.2.3",
                   source_metadata=True, **wheel):
    wheels = [make_wheel(directory, tag, **wheel) for tag in tags]
    source = directory / "fridica-1.2.3.tar.gz"
    with tarfile.open(source, "w:gz") as archive:
        if source_metadata:
            metadata = f"Name: {source_name}\nVersion: {source_version}\n".encode()
            member = tarfile.TarInfo("fridica-1.2.3/PKG-INFO")
            member.size = len(metadata)
            archive.addfile(member, io.BytesIO(metadata))
    return wheels, source


def test_verify_artifacts(tmp_path):
    make_artifacts(tmp_path)
    release.verify_artifacts(tmp_path, "v1.2.3")


@pytest.mark.parametrize("os_choice, tags", [
    ("Ubuntu", PLATFORM_TAGS[:2]), ("Linux", PLATFORM_TAGS[:2]), ("MacOS", PLATFORM_TAGS[2:]),
])
def test_verify_artifacts_for_one_os(tmp_path, os_choice, tags):
    make_artifacts(tmp_path, tags=tags)
    release.verify_artifacts(tmp_path, "v1.2.3", os_choice)


@pytest.mark.parametrize("changes, error", [
    ({"scripts": ()}, "fridica executable"),
    ({"metadata": False}, "wheel metadata"),
    ({"source_metadata": False}, "source distribution metadata"),
    ({"name": "other"}, "name/version"),
    ({"source_name": "other"}, "name/version"),
    ({"version": "1.2.4"}, "name/version"),
    ({"source_version": "1.2.3.dev1"}, "name/version"),
    ({"tags": PLATFORM_TAGS[:3]}, "missing macosx arm64"),
    ({"tags": PLATFORM_TAGS + ["py3-none-any"]}, "exactly one required platform"),
    ({"tags": PLATFORM_TAGS[:3] + ["cp311-cp311-macosx_11_0_arm64"]}, "exactly one required platform"),
])
def test_verify_artifacts_rejects_bad_content(tmp_path, changes, error):
    make_artifacts(tmp_path, **changes)
    with pytest.raises(ValueError, match=error):
        release.verify_artifacts(tmp_path, "v1.2.3")


def test_verify_rejects_duplicate_platform_and_wrong_os(tmp_path):
    make_artifacts(tmp_path)
    make_wheel(tmp_path, "py3-none-manylinux_2_17_x86_64")
    with pytest.raises(ValueError, match="exactly one wheel for manylinux x86_64"):
        release.verify_artifacts(tmp_path, "v1.2.3")
    with pytest.raises(ValueError, match="--os"):
        release.verify_artifacts(tmp_path, "v1.2.3", "Windows")


@pytest.mark.parametrize("duplicate", [False, True])
def test_verify_requires_one_source_distribution(tmp_path, duplicate):
    _, source = make_artifacts(tmp_path)
    if duplicate:
        (tmp_path / "extra.tar.gz").write_bytes(source.read_bytes())
    else:
        source.unlink()
    with pytest.raises(ValueError, match="exactly one source distribution"):
        release.verify_artifacts(tmp_path, "v1.2.3")


def test_stamp_sets_one_version_everywhere(tmp_path):
    root = Path(__file__).parents[1]
    for name in ("pyproject.toml", "Cargo.toml", "Cargo.lock"):
        (tmp_path / name).write_text((root / name).read_text())
    assert release.stamp(tmp_path, "v2.3.4") == "2.3.4"
    assert '\nversion = "2.3.4"\n' in (tmp_path / "pyproject.toml").read_text()
    assert (tmp_path / "Cargo.toml").read_text().startswith('[package]\nname = "fridica"\nversion = "2.3.4"\n')
    assert 'name = "fridica"\nversion = "2.3.4"\n' in (tmp_path / "Cargo.lock").read_text()
    with pytest.raises(ValueError, match="vMAJOR"):
        release.stamp(tmp_path, "2.3.4")
    (tmp_path / "Cargo.lock").write_text("")
    with pytest.raises(ValueError, match="Cargo.lock"):
        release.stamp(tmp_path, "v2.3.5")


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
