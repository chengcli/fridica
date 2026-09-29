"""No-follow settings walks accept only macOS's root-owned /private links."""
import importlib.util
import os
from pathlib import Path
import stat
import tempfile
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("settings_paths", ROOT / "src/exec/settings_paths.py")
paths = importlib.util.module_from_spec(spec)
spec.loader.exec_module(paths)


def link(uid):
    return os.stat_result((stat.S_IFLNK | 0o755, 0, 0, 1, uid, 0, 11, 0, 0, 0))


class SystemPathTests(unittest.TestCase):
    def resolve(self, value, platform="darwin", uid=0, target="private/etc"):
        with mock.patch.object(paths.sys, "platform", platform), \
                mock.patch.object(paths.os, "lstat", return_value=link(uid)), \
                mock.patch.object(paths.os, "readlink", return_value=target):
            return paths.system_path(value)

    def test_only_root_owned_private_links_are_followed_on_macos(self):
        self.assertEqual(self.resolve("/etc/codex/config.toml"), "/private/etc/codex/config.toml")
        self.assertEqual(self.resolve("/var/x", target="private/var"), "/private/var/x")
        self.assertEqual(self.resolve("/etc/codex/config.toml", uid=501), "/etc/codex/config.toml")
        self.assertEqual(self.resolve("/etc/codex/config.toml", target="/Users/evil"), "/etc/codex/config.toml")
        self.assertEqual(self.resolve("/Users/me/.codex", target="private/Users"), "/Users/me/.codex")
        self.assertEqual(self.resolve("/etc/codex/config.toml", platform="linux"), "/etc/codex/config.toml")

    def test_other_ancestor_symlinks_are_still_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            real = Path(os.path.realpath(tmp))
            (real / "target").mkdir()
            (real / "link").symlink_to(real / "target")
            os.close(paths.directory(str(real / "target")))
            with self.assertRaises(OSError):
                paths.directory(str(real / "link"))


if __name__ == "__main__":
    unittest.main()
