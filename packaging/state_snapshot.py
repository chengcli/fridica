#!/usr/bin/env python3
"""Snapshot stopped state, or verify and unpack it into a NEW recovery directory.

No in-place downgrade/overwrite. Point the stopped service at the recovered DB
only after reviewing its matching config and the loss of subsequent writes.
"""
import argparse
from contextlib import closing, contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import stat


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


@contextmanager
def state_lock(database):
    # Matches Rust Path::with_extension("lock"); never replace this lock inode.
    descriptor = os.open(database.with_suffix(".lock"), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        metadata = os.fstat(descriptor)
        if (not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid()
                or metadata.st_nlink != 1 or metadata.st_mode & 0o077):
            raise ValueError("invalid state lock")
        fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield
    finally:
        os.close(descriptor)


def save(database, config, destination):
    database, config = database.resolve(strict=True), config.resolve(strict=True)
    with state_lock(database):
        before = config.read_bytes()
        destination.mkdir(mode=0o700)
        db = destination / "state.sqlite3"
        db.touch(mode=0o600, exist_ok=False)
        with closing(sqlite3.connect(database.as_uri() + "?mode=ro", uri=True)) as source:
            with closing(sqlite3.connect(db)) as target:
                source.backup(target)
                if target.execute("PRAGMA integrity_check").fetchone() != ("ok",):
                    raise ValueError("snapshot integrity check failed")
        if config.read_bytes() != before:
            raise ValueError("configuration changed during snapshot; discard incomplete snapshot")
        cfg = destination / "config.toml"
        cfg.touch(mode=0o600, exist_ok=False)
        cfg.write_bytes(before)
        manifest = {"database": str(database), "config": str(config),
                    "files": {name: digest(destination / name) for name in ("state.sqlite3", "config.toml")}}
        path = destination / "snapshot.json"
        path.touch(mode=0o600, exist_ok=False)
        path.write_text(json.dumps(manifest, indent=2) + "\n")
        for path in (db, cfg, path):
            with path.open("rb") as stream:
                os.fsync(stream.fileno())
        sync_directory(destination)
        sync_directory(destination.parent)


def sync_directory(path):
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def restore(snapshot, destination):
    manifest = json.loads((snapshot / "snapshot.json").read_text())
    if set(manifest["files"]) != {"state.sqlite3", "config.toml"}:
        raise ValueError("invalid snapshot inventory")
    for name, expected in manifest["files"].items():
        if digest(snapshot / name) != expected:
            raise ValueError("snapshot hash mismatch")
    # Reuse the SQLite backup API and integrity check, never copy WAL files.
    save(snapshot / "state.sqlite3", snapshot / "config.toml", destination)
    print("Recovered into a new directory. Keep service stopped; review config and set its state.path to:")
    print(destination.resolve() / "state.sqlite3")
    print("Original config location (relative paths resolve there): " + manifest["config"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    backup = commands.add_parser("backup")
    backup.add_argument("--database", type=Path, required=True)
    backup.add_argument("--config", type=Path, required=True)
    backup.add_argument("--output", type=Path, required=True)
    recovery = commands.add_parser("restore")
    recovery.add_argument("--snapshot", type=Path, required=True)
    recovery.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "backup":
        save(args.database, args.config, args.output)
    else:
        restore(args.snapshot, args.output)


if __name__ == "__main__":
    main()
