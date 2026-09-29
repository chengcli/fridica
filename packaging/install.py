#!/usr/bin/env python3
"""Verify and unpack a native candidate into a new private directory (stdlib only)."""
import argparse
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import tarfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def verify(archive, expected):
    data = Path(archive).read_bytes()
    if digest(data) != expected:
        raise ValueError("archive SHA-256 mismatch")
    payload = {}
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as stream:
        for entry in stream:
            path = PurePosixPath(entry.name)
            if (not entry.isfile() or path.is_absolute() or ".." in path.parts
                    or str(path) != entry.name or entry.name in payload
                    or entry.mode not in (0o644, 0o755) or entry.size > 256 * 1024 * 1024):
                raise ValueError("invalid archive member")
            payload[entry.name] = (stream.extractfile(entry).read(), entry.mode)
    manifest = json.loads(payload["build-manifest.json"][0])
    files = manifest["files"]
    if set(payload) != set(files) | {"build-manifest.json"}:
        raise ValueError("manifest inventory mismatch")
    for name, info in files.items():
        data, mode = payload[name]
        if info != {"sha256": digest(data), "mode": mode, "size": len(data)}:
            raise ValueError(f"payload mismatch: {name}")
    # Reject file/directory collisions before creating anything.
    for name in payload:
        if any(str(parent) in payload for parent in PurePosixPath(name).parents):
            raise ValueError("conflicting archive paths")
    return manifest, payload


def install(archive, expected, destination):
    manifest, payload = verify(archive, expected)
    destination = Path(destination)
    destination.mkdir(mode=0o700)  # must be new, including no symlink
    for name, (data, mode) in sorted(payload.items()):
        path = destination / name
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("xb") as stream:
            stream.write(data)
        path.chmod(mode)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("destination", type=Path)
    parser.add_argument("--sha256", required=True, help="expected digest from the trusted build")
    args = parser.parse_args()
    print(json.dumps(install(args.archive, args.sha256, args.destination)["build"], indent=2))


if __name__ == "__main__":
    main()
