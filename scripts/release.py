"""Local, side-effect-free release planning and artifact verification."""

import argparse
from email.parser import BytesParser
import json
import os
from pathlib import Path
import re
import subprocess
import tarfile
import zipfile


TAG_PATTERN = re.compile(r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def parse_tag(tag: str) -> tuple[int, int, int]:
    match = TAG_PATTERN.fullmatch(tag)
    if match is None:
        raise ValueError("release tag must have the form vMAJOR.MINOR.PATCH")
    return tuple(int(part) for part in match.groups())


def next_tag(tags: list[str], labels: list[str]) -> str:
    selected = set(labels) & {"release:major", "release:minor", "release:patch"}
    if len(selected) > 1:
        raise ValueError("only one release bump label is allowed")
    versions = [parse_tag(tag) for tag in tags if TAG_PATTERN.fullmatch(tag)]
    if not versions:
        return "v0.1.0"
    major, minor, patch = max(versions)
    bump = next(iter(selected), "release:patch")
    if bump == "release:major":
        major, minor, patch = major + 1, 0, 0
    elif bump == "release:minor":
        minor, patch = minor + 1, 0
    else:
        patch += 1
    return f"v{major}.{minor}.{patch}"


def verify_artifacts(directory: Path, tag: str) -> None:
    parse_tag(tag)
    wheels = list(directory.glob("*.whl"))
    sources = list(directory.glob("*.tar.gz"))
    if len(wheels) != 1 or len(sources) != 1:
        raise ValueError("expected exactly one wheel and one source distribution")
    with zipfile.ZipFile(wheels[0]) as archive:
        names = [name for name in archive.namelist() if name.endswith(".dist-info/METADATA")]
        contents = archive.namelist()
        if len(names) != 1 or "fridica/manifest.yaml" not in contents or "fridica/contract.md" not in contents:
            raise ValueError("wheel metadata, Slack manifest, or agent contract is missing")
        wheel_metadata = archive.read(names[0])
    with tarfile.open(sources[0]) as archive:
        names = [member for member in archive.getmembers() if member.name.count("/") == 1 and member.name.endswith("/PKG-INFO")]
        if len(names) != 1:
            raise ValueError("source distribution metadata is missing")
        source_metadata = archive.extractfile(names[0]).read()
    for metadata in (wheel_metadata, source_metadata):
        parsed = BytesParser().parsebytes(metadata)
        if parsed["Name"] != "fridica" or parsed["Version"] != tag[1:]:
            raise ValueError("artifact name/version does not match the requested release")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("next")
    verify = commands.add_parser("verify")
    verify.add_argument("--tag", required=True)
    verify.add_argument("--dist", type=Path, default=Path("dist"))
    args = parser.parse_args()
    if args.command == "verify":
        verify_artifacts(args.dist, args.tag)
        print(f"Verified artifacts for {args.tag}")
        return
    labels = [item["name"] for item in json.loads(os.environ.get("LABELS_JSON", "[]"))]
    tags = subprocess.check_output(["git", "tag", "--list"], text=True).splitlines()
    current = subprocess.check_output(["git", "tag", "--points-at", "HEAD"], text=True).splitlines()
    existing = sorted(tag for tag in current if TAG_PATTERN.fullmatch(tag))
    if len(existing) > 1:
        raise ValueError("multiple release tags point to this commit")
    tag = existing[0] if existing else next_tag(tags, labels)
    print(f"tag={tag}")
    print(f"create={'false' if existing else 'true'}")


if __name__ == "__main__":
    main()
