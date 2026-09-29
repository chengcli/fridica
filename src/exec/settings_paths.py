"""Shared no-follow file reads for trusted worker settings helpers."""
import json
import os
import stat
import sys


class Refused(Exception):
    pass


def path(value):
    if (not isinstance(value, str) or not value.startswith("/") or "\0" in value
            or any(p in (".", "..") for p in value.split("/"))):
        raise Refused()
    # Linux treats // like /; normpath deliberately preserves a double leading
    # slash, which would otherwise bypass the overlap checks below.
    return "/" + "/".join(part for part in value.split("/") if part)


def within(value, root):
    return value == root or value.startswith(root.rstrip("/") + "/")


def system_path(value):
    """Follow only macOS's root-owned /etc, /tmp and /var links into /private."""
    if sys.platform == "darwin":
        head = value[1:].partition("/")[0]
        if head in ("etc", "tmp", "var"):
            info = os.lstat("/" + head)
            if (stat.S_ISLNK(info.st_mode) and info.st_uid == 0
                    and os.readlink("/" + head) == "private/" + head):
                return "/private" + value
    return value


def directory(value, create=False):
    """Never follow even an ancestor symlink or race a path-based mkdir."""
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in system_path(value).split("/")[1:]:
            if not part:
                continue
            if create:
                try:
                    os.mkdir(part, 0o700, dir_fd=fd)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = child
        return fd
    except BaseException:
        os.close(fd)
        raise


def regular(value):
    parent = directory(os.path.dirname(value))
    try:
        fd = os.open(os.path.basename(value), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=parent)
    finally:
        os.close(parent)
    info = os.fstat(fd)
    if not stat.S_ISREG(info.st_mode) or info.st_nlink != 1:
        os.close(fd)
        raise Refused()
    return fd



def worker_environment(excluded):
    # SSH login/AcceptEnv can introduce secrets absent from outgoing variables.
    return {
        key: value for key, value in os.environ.items()
        if key not in excluded and "SLACK" not in key.upper()
        and not key.upper().startswith("FRIDICA_")
        and not value.startswith(("xoxp-", "xoxb-", "xapp-", "xoxe-"))
    }
