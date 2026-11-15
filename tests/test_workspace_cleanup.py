"""Retention deletes only expired scratch data inside an idle slot."""
import importlib.util
import os
from pathlib import Path
import tempfile
import time
import unittest

spec = importlib.util.spec_from_file_location(
    'cleanup', Path(__file__).resolve().parents[1] / 'src/exec/cleanup.py')
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name).resolve()
        self.slot = self.root / 'worker1'
        self.slot.mkdir()
        self.now = time.time()

    def file(self, name, age=helper.WEEK + 10):
        p = self.slot / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text('keep or expire')
        os.utime(p, (self.now - age, self.now - age))
        return p

    def test_expiry_boundary_recent_access_and_sibling(self):
        old = self.file('nested/old')
        recent = self.file('recent', 100)
        boundary = self.file('boundary', helper.WEEK)
        read = self.file('read')
        os.utime(read, (self.now, self.now - 2 * helper.WEEK))
        sibling = self.root / 'worker2'
        sibling.mkdir()
        outside = sibling / 'old'
        outside.write_text('untouched')
        os.utime(outside, (0, 0))
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 1)
        self.assertFalse(old.exists())
        self.assertFalse(old.parent.exists())
        for p in (recent, boundary, read, outside, self.slot):
            self.assertTrue(p.exists())
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)

    def test_repositories_worktrees_and_pinned_directories_survive(self):
        for directory, marker in [('repo', '.git'), ('worktree', '.git'), ('saved', '.fridica-keep')]:
            old = self.file(directory + '/old')
            path = old.parent / marker
            if directory == 'repo':
                path.mkdir()
            else:
                path.write_text('marker')
        self.file('scratch')
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 1)
        for directory in ('repo', 'worktree', 'saved'):
            self.assertTrue((self.slot / directory / 'old').exists())

    def test_no_symlink_or_hardlink_traversal(self):
        external = self.root / 'external'
        external.mkdir()
        victim = external / 'old'
        victim.write_text('untouched')
        os.utime(victim, (0, 0))
        (self.slot / 'dirlink').symlink_to(external)
        (self.slot / 'filelink').symlink_to(victim)
        os.link(victim, self.slot / 'hardlink')
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)
        self.assertEqual(victim.read_text(), 'untouched')
        alias = self.root / 'alias'
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(OSError):
            helper.cleanup(str(alias / 'worker1'), 1, self.now)

    def test_root_pin_and_repository_ancestor(self):
        old = self.file('old')
        (self.slot / '.fridica-keep').touch()
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)
        (self.slot / '.fridica-keep').unlink()
        (self.root / '.git').mkdir()
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)
        self.assertTrue(old.exists())

    def test_bare_repository_and_ancestor_pin(self):
        old = self.file('bare/old')
        for name in ('HEAD', 'objects', 'refs'):
            (old.parent / name).touch()
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)
        scratch = self.file('scratch')
        (self.root / '.fridica-keep').touch()
        self.assertEqual(helper.cleanup(str(self.slot), 1, self.now), 0)
        self.assertTrue(scratch.exists())

    def test_shared_or_mismatched_root_refused(self):
        for root, slot in [(self.root, 1), (self.slot, 2), (self.slot, 0)]:
            with self.assertRaises(ValueError):
                helper.cleanup(str(root), slot, self.now)


if __name__ == '__main__':
    unittest.main()
