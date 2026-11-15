"""Remove expired scratch files from an idle worker slot, without following links."""
import json
import os
import stat
import sys
import time

WEEK = 7 * 24 * 60 * 60
FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC


def protected(names):
    return ('.git' in names or '.fridica-keep' in names
            or {'HEAD', 'objects', 'refs'}.issubset(names))


def clean(fd, cutoff, device):
    # A .git file denotes a worktree; a directory denotes a normal checkout.
    names = os.listdir(fd)
    if protected(names):
        return 0
    removed = 0
    for name in names:
        try:
            info = os.stat(name, dir_fd=fd, follow_symlinks=False)
            if info.st_dev != device:
                continue
            if stat.S_ISDIR(info.st_mode):
                child = os.open(name, FLAGS, dir_fd=fd)
                try:
                    current = os.fstat(child)
                    if (current.st_dev, current.st_ino) != (info.st_dev, info.st_ino):
                        continue
                    count = clean(child, cutoff, device)
                finally:
                    os.close(child)
                removed += count
                # Only prune directories emptied by this pass, never the slot root.
                if count:
                    os.rmdir(name, dir_fd=fd)
            elif stat.S_ISREG(info.st_mode) and info.st_nlink == 1:
                if max(info.st_mtime, info.st_atime) < cutoff:
                    os.unlink(name, dir_fd=fd)
                    removed += 1
        except OSError:
            # Concurrent changes and inaccessible files are left for a later job.
            continue
    return removed


def cleanup(workspace, slot, now=None):
    workspace = os.path.expanduser(workspace)
    if (not os.path.isabs(workspace) or '..' in workspace.split('/')
            or os.path.basename(workspace) != f'worker{slot}' or slot < 1):
        raise ValueError('invalid slot')
    # Resolve only the platform's trusted /tmp and /var aliases on macOS.
    if sys.platform == 'darwin':
        for alias in ('/tmp', '/var'):
            if workspace.startswith(alias + '/'):
                workspace = os.path.realpath(alias) + workspace[len(alias):]
                break
    fd = os.open('/', FLAGS)
    try:
        for part in workspace.split('/')[1:]:
            if not part or part == '.':
                continue
            if protected(os.listdir(fd)):
                return 0  # The slot itself lives inside a checkout.
            child = os.open(part, FLAGS, dir_fd=fd)
            os.close(fd)
            fd = child
        return clean(fd, (time.time() if now is None else now) - WEEK, os.fstat(fd).st_dev)
    finally:
        os.close(fd)


if __name__ == '__main__':
    try:
        request = json.loads(sys.argv[1])
        print(cleanup(request['workspace'], request['slot']))
    except (OSError, ValueError, RecursionError):
        sys.exit(1)
