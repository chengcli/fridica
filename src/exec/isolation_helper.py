"""Fixed launch helper: no backend startup before the private mount view.

All directory traversal uses no-follow descriptors. Only approved workspace and
backend-state descriptors survive exec; bwrap consumes them as mount sources.
Diagnostics deliberately exclude paths, settings, credentials and command text.
"""
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


def directory(value, create=False):
    """Never follow even an ancestor symlink or race a path-based mkdir."""
    fd = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in value.split("/")[1:]:
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


def default(value, data):
    parent = directory(os.path.dirname(value), create=True)
    try:
        try:
            fd = os.open(os.path.basename(value), os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600, dir_fd=parent)
        except FileExistsError:
            return
        with os.fdopen(fd, "w") as output:
            output.write(data)
    finally:
        os.close(parent)


def main():
    request = json.loads(sys.argv[1])
    if sys.platform != "linux":
        raise Refused()
    remote = request["home"] is None
    home = path(os.environ["HOME"] if remote else request["home"])

    def target(value):
        if remote and isinstance(value, str) and value.startswith("~/"):
            value = home + "/" + value[2:]
        return path(value)

    workspace = target(request["workspace"])
    private = [target(p) for p in request["private"]]
    if home == "/" or not private or not sys.argv[2:]:
        raise Refused()
    # SSH login/AcceptEnv can introduce credentials absent from the daemon's
    # outgoing environment. Scrub on the target as well, before backend exec.
    excluded = request.get("excluded_env", [])
    environment = {
        key: value for key, value in os.environ.items()
        if key not in excluded and "SLACK" not in key.upper()
        and not key.upper().startswith("FRIDICA_")
        and not value.startswith(("xoxp-", "xoxb-", "xapp-", "xoxe-"))
    }
    # Resolve private aliases too, but never use them as worker mount sources.
    private += [path(os.path.realpath(p)) for p in private]
    # A second hard-link name could live outside every masked directory. Do not
    # claim isolation of such files, including owner-created aliases.
    for value in private:
        try:
            info = os.stat(value)
        except FileNotFoundError:
            continue
        if stat.S_ISDIR(info.st_mode) or (stat.S_ISREG(info.st_mode) and info.st_nlink != 1):
            raise Refused()
    masks = sorted(set(os.path.dirname(p) for p in private), key=lambda p: (len(p), p))
    selected = []
    for mask in masks:
        if mask == "/":
            raise Refused()
        if not any(within(mask, parent) for parent in selected):
            selected.append(mask)
    state = [home + "/.codex", home + "/.claude", home + "/.claude.json"]
    # A remount of backend state or a workspace must not re-expose private files.
    if any(within(secret, root) for secret in private for root in [workspace] + state):
        raise Refused()
    # The configured private directories must already exist before admission.
    for mask in selected:
        os.close(directory(mask))
    os.close(directory(home))
    os.close(directory(home + "/.codex", create=True))
    os.close(directory(home + "/.claude/hooks", create=True))
    default(home + "/.codex/config.toml", "")
    default(home + "/.claude/settings.json", "{}")
    default(home + "/.claude/settings.local.json", "{}")

    fds = []
    words = ["/usr/bin/bwrap", "--die-with-parent", "--new-session", "--unshare-user", "--unshare-pid",
             "--cap-drop", "ALL", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev",
             "--proc", "/proc", "--tmpfs", "/tmp"]
    for mask in selected:
        words += ["--tmpfs", mask]

    def mount(fd, dest, readonly=False):
        fds.append(fd)
        words.extend(["--ro-bind-fd" if readonly else "--bind-fd", str(fd), dest])

    mount(directory(workspace, create=request["create"]), workspace)
    for value in state:
        try:
            fd = regular(value) if value.endswith(".json") else directory(value)
        except FileNotFoundError:
            continue
        mount(fd, value)
    for suffix in [".codex/config.toml", ".claude/settings.json", ".claude/settings.local.json", ".claude/hooks"]:
        value = home + "/" + suffix
        mount(directory(value) if suffix.endswith("hooks") else regular(value), value, readonly=True)
    # Make empty private parents read-only after child bind targets are created.
    # Remount is intentionally nonrecursive: approved workspaces stay writable.
    for mask in selected:
        words += ["--remount-ro", mask]
    words += ["--chdir", workspace, "--"] + sys.argv[2:]
    for fd in fds:
        os.set_inheritable(fd, True)
    os.execve(words[0], words, environment)


try:
    main()
except (OSError, ValueError, KeyError, TypeError, Refused):
    sys.stderr.write("fridica worker isolation: setup refused\n")
    sys.exit(97)
