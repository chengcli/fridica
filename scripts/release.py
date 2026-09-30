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


# Native wheel platforms: one py3-none-<platform> wheel each (no per-Python builds).
PLATFORMS = {
    "linux": {"manylinux x86_64": ("manylinux", "_x86_64"), "manylinux aarch64": ("manylinux", "_aarch64")},
    "macos": {"macosx x86_64": ("macosx", "_x86_64"), "macosx arm64": ("macosx", "_arm64")},
}
SCRIPTS = ("fridica", "fridica-overseer")


def required_platforms(os_choice: str) -> dict[str, tuple[str, str]]:
    choices = {"both": ["linux", "macos"], "linux": ["linux"], "ubuntu": ["linux"], "macos": ["macos"]}
    if os_choice.lower() not in choices:
        raise ValueError("--os must be Both, Linux/Ubuntu or MacOS")
    return {name: rule for key in choices[os_choice.lower()] for name, rule in PLATFORMS[key].items()}


def stamp(root: Path, tag: str) -> str:
    """Write the release version into pyproject.toml, Cargo.toml and Cargo.lock."""
    version = ".".join(map(str, parse_tag(tag)))
    edits = [
        (root / "pyproject.toml", r'(?m)^(version = ")[^"]*(")'),
        (root / "Cargo.toml", r'(?m)\A(\[package\]\nname = "fridica"\nversion = ")[^"]*(")'),
        (root / "Cargo.lock", r'(?m)^(name = "fridica"\nversion = ")[^"]*(")'),
    ]
    for path, pattern in edits:
        text, count = re.subn(pattern, rf"\g<1>{version}\g<2>", path.read_text(), count=1)
        if count != 1:
            raise ValueError(f"cannot find the fridica version in {path.name}")
        path.write_text(text)
    return version


def verify_artifacts(directory: Path, tag: str, os_choice: str = "Both") -> None:
    parse_tag(tag)
    required = required_platforms(os_choice)
    wheels = sorted(directory.glob("*.whl"))
    sources = list(directory.glob("*.tar.gz"))
    if len(sources) != 1:
        raise ValueError("expected exactly one source distribution")
    found = {}
    metadata = []
    for wheel in wheels:
        with zipfile.ZipFile(wheel) as archive:
            names = archive.namelist()
            meta = [name for name in names if name.endswith(".dist-info/METADATA")]
            info = [name for name in names if name.endswith(".dist-info/WHEEL")]
            if len(meta) != 1 or len(info) != 1:
                raise ValueError(f"wheel metadata is missing from {wheel.name}")
            data = meta[0].split(".dist-info/")[0] + ".data/scripts/"
            if not all(data + script in names for script in SCRIPTS):
                raise ValueError(f"{wheel.name} lacks the fridica and fridica-overseer executables")
            tags = [line.split(":", 1)[1].strip() for line in archive.read(info[0]).decode().splitlines()
                    if line.startswith("Tag:")]
            metadata.append(archive.read(meta[0]))
        platforms = {name for name, (kind, arch) in required.items()
                     for value in tags if value.startswith("py3-none-" + kind) and value.endswith(arch)}
        if not tags or not all(value.startswith("py3-none-") for value in tags) or len(platforms) != 1:
            raise ValueError(f"{wheel.name} is not a py3-none wheel for exactly one required platform")
        platform = platforms.pop()
        if platform in found:
            raise ValueError(f"expected exactly one wheel for {platform}")
        found[platform] = wheel
    missing = sorted(set(required) - set(found))
    if missing or len(found) != len(wheels):
        raise ValueError("expected exactly one wheel per platform; missing " + (", ".join(missing) or "none"))
    with tarfile.open(sources[0]) as archive:
        names = [member for member in archive.getmembers() if member.name.count("/") == 1 and member.name.endswith("/PKG-INFO")]
        if len(names) != 1:
            raise ValueError("source distribution metadata is missing")
        metadata.append(archive.extractfile(names[0]).read())
    for data in metadata:
        parsed = BytesParser().parsebytes(data)
        if parsed["Name"] != "fridica" or parsed["Version"] != tag[1:]:
            raise ValueError("artifact name/version does not match the requested release")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("next")
    verify = commands.add_parser("verify")
    verify.add_argument("--tag", required=True)
    verify.add_argument("--dist", type=Path, default=Path("dist"))
    verify.add_argument("--os", default="Both", help="Both, Linux/Ubuntu or MacOS")
    stamp_command = commands.add_parser("stamp")
    stamp_command.add_argument("--tag", required=True)
    stamp_command.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    if args.command == "verify":
        verify_artifacts(args.dist, args.tag, args.os)
        print(f"Verified artifacts for {args.tag}")
        return
    if args.command == "stamp":
        print(f"version={stamp(args.root, args.tag)}")
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
