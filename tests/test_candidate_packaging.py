"""Native packaging and stopped-state recovery failures; no live installation."""
import importlib.util
import io
from pathlib import Path
import sqlite3
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, ROOT / path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


installer = module("candidate_install", "packaging/install.py")
packager = module("candidate_package", "scripts/package_candidate.py")
snapshot = module("candidate_snapshot", "packaging/state_snapshot.py")


class PackagingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)

    def archive(self, files):
        manifest = {"build": {"source_id": "a" * 64}, "files": {
            name: {"sha256": installer.digest(data), "size": len(data), "mode": mode}
            for name, (data, mode) in files.items()}}
        payload = dict(files, **{"build-manifest.json": (packager.canonical(manifest), 0o644)})
        data = packager.archive_bytes(payload)
        path = self.root / "candidate.tar.gz"
        path.write_bytes(data)
        return path, installer.digest(data), payload

    def test_reproducible_archive_and_install_never_replace_existing_paths(self):
        archive, digest, payload = self.archive({"bin/candidate": (b"fake executable", 0o755)})
        self.assertEqual(archive.read_bytes(), packager.archive_bytes(dict(reversed(list(payload.items())))))
        install = self.root / "install"
        installer.install(archive, digest, install)
        self.assertEqual((install / "bin/candidate").stat().st_mode & 0o777, 0o755)
        with self.assertRaises(FileExistsError):
            installer.install(archive, digest, install)
        link = self.root / "link"
        link.symlink_to(install, target_is_directory=True)
        with self.assertRaises(FileExistsError):
            installer.install(archive, digest, link)
        with self.assertRaises(ValueError):
            installer.install(archive, "0" * 64, self.root / "corrupt")
        self.assertFalse((self.root / "corrupt").exists())

    def test_archive_rejects_traversal_links_duplicates_and_file_collisions(self):
        for name in ("../outside", "/absolute", "dir/../outside", "./not-canonical"):
            archive, digest, _ = self.archive({name: (b"bad", 0o644)})
            with self.assertRaises(ValueError):
                installer.verify(archive, digest)
        archive, digest, _ = self.archive({"file": (b"x", 0o644), "file/child": (b"y", 0o644)})
        with self.assertRaises(ValueError):
            installer.verify(archive, digest)
        for duplicate in (False, True):
            raw = io.BytesIO()
            with tarfile.open(fileobj=raw, mode="w:gz") as stream:
                entry = tarfile.TarInfo("x")
                entry.mode = 0o644
                if duplicate:
                    stream.addfile(entry, io.BytesIO())
                else:
                    entry.type = tarfile.SYMTYPE
                    entry.linkname = "elsewhere"
                stream.addfile(entry, io.BytesIO())
            archive.write_bytes(raw.getvalue())
            with self.assertRaises(ValueError):
                installer.verify(archive, installer.digest(archive.read_bytes()))

    def test_manifest_detects_tampered_missing_and_extra_payloads(self):
        archive, _, payload = self.archive({"bin/candidate": (b"original", 0o755)})
        for changed in (dict(payload, **{"bin/candidate": (b"modified", 0o755)}),
                        {name: value for name, value in payload.items() if name != "bin/candidate"},
                        dict(payload, **{"extra": (b"surprise", 0o644)})):
            data = packager.archive_bytes(changed)
            archive.write_bytes(data)
            with self.assertRaises(ValueError):
                installer.verify(archive, installer.digest(data))

    def test_snapshot_includes_committed_wal_and_restore_is_explicit_new_directory(self):
        database = self.root / "state.db"
        config = self.root / "config.toml"
        config.write_text("# retained config\n")
        source = sqlite3.connect(database)
        self.addCleanup(source.close)
        source.execute("PRAGMA journal_mode=WAL")
        source.execute("CREATE TABLE sample(value TEXT)")
        source.execute("INSERT INTO sample VALUES('committed')")
        source.commit()
        source.execute("INSERT INTO sample VALUES('uncommitted')")
        backup = self.root / "snapshot"
        snapshot.save(database, config, backup)
        recovered = self.root / "recovered"
        snapshot.restore(backup, recovered)
        with sqlite3.connect(recovered / "state.sqlite3") as conn:
            self.assertEqual(conn.execute("SELECT * FROM sample").fetchall(), [("committed",)])
        self.assertEqual((recovered / "config.toml").read_bytes(), config.read_bytes())
        self.assertEqual(recovered.stat().st_mode & 0o777, 0o700)
        for name in ("state.sqlite3", "config.toml", "snapshot.json"):
            self.assertEqual((recovered / name).stat().st_mode & 0o777, 0o600)
        with self.assertRaises(FileExistsError):
            snapshot.restore(backup, recovered)
        (backup / "config.toml").write_text("tampered")
        with self.assertRaises(ValueError):
            snapshot.restore(backup, self.root / "bad")
        self.assertFalse((self.root / "bad").exists())

    def test_snapshot_refuses_running_state_and_incomplete_snapshot(self):
        database = self.root / "state.db"
        with sqlite3.connect(database) as conn:
            conn.execute("CREATE TABLE sample(value)")
        config = self.root / "config.toml"
        config.write_text("# config")
        with snapshot.state_lock(database):
            with self.assertRaises(BlockingIOError):
                snapshot.save(database, config, self.root / "live")
        self.assertFalse((self.root / "live").exists())
        with self.assertRaises(FileNotFoundError):
            snapshot.restore(self.root, self.root / "incomplete")


if __name__ == "__main__":
    unittest.main()
