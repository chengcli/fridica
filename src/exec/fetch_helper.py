"""Private target-host utility embedded by Rust; never an agent-authored script.

Git runs only in a private staging directory. Publication never follows a
workspace symlink or overwrites an existing file. No credentials are inherited.
"""
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading


STOP_SIGNALS = (signal.SIGTERM, signal.SIGINT, signal.SIGHUP)


class Interrupted(Exception):
    """A stop signal arrived. Not InterruptedError: Python's selectors treat
    that one as a retried EINTR, so communicate() would run to its deadline
    and a SIGKILLed helper would leave the Git process group running."""


def interrupt(_signal, _frame):
    # The first signal starts shutdown; later ones (stdin EOF plus the owner's or
    # watchdog's SIGTERM) must not interrupt removal of the staging directory.
    for number in STOP_SIGNALS:
        signal.signal(number, signal.SIG_IGN)
    raise Interrupted("fetch interrupted")


def watch_input():
    # The Rust process owner holds stdin until completion; EOF also covers a
    # daemon crash or a lost SSH channel, including hosts without setsid/timeout.
    try:
        while os.read(0, 1):
            pass
        os.kill(os.getpid(), signal.SIGTERM)
    except OSError:
        pass


def git(argv, cwd, deadline):
    process = subprocess.Popen(argv, cwd=cwd, env={
        "PATH": "/usr/bin:/bin", "HOME": cwd,
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": "/dev/null",
        "GIT_CONFIG_COUNT": "0", "GIT_CONFIG_PARAMETERS": "",
        "GIT_TERMINAL_PROMPT": "0", "GIT_ASKPASS": "/usr/bin/false",
        "GIT_SSH_COMMAND": "/usr/bin/false",
    }, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
       start_new_session=True)
    try:
        output, _ = process.communicate(timeout=deadline)
        if process.returncode or len(output) > 4096:
            raise RuntimeError("git failed")
        return output
    finally:
        # This group may include git-remote-https even after Git itself exited.
        # Block interruption during cleanup so a second signal cannot strand it.
        old = signal.signal(signal.SIGTERM, signal.SIG_IGN)
        try:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        finally:
            signal.signal(signal.SIGTERM, old)


def directory(path, create):
    """Resolve owner-configured roots, then open each component without links."""
    canonical = os.path.realpath(os.path.expanduser(path))
    current = os.open("/", os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in canonical.split("/")[1:]:
            if not part:
                continue
            if create:
                try:
                    os.mkdir(part, 0o700, dir_fd=current)
                except FileExistsError:
                    pass
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=current)
            os.close(current)
            current = child
        return current
    except BaseException:
        os.close(current)
        raise


def publish(source, destination):
    for entry in os.scandir(source):
        if entry.is_dir(follow_symlinks=False):
            os.mkdir(entry.name, 0o700, dir_fd=destination)
            child = os.open(entry.name, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC,
                            dir_fd=destination)
            try:
                publish(entry.path, child)
            finally:
                os.close(child)
        elif entry.is_file(follow_symlinks=False):
            fd = os.open(entry.name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                         0o600, dir_fd=destination)
            with os.fdopen(fd, "wb") as output, open(entry.path, "rb") as data:
                shutil.copyfileobj(data, output, 1024 * 1024)
        else:
            raise RuntimeError("invalid fetched file")


def main():
    request = json.loads(sys.argv[1])
    # Repeat structural checks at the target boundary, without accepting a URL,
    # shell fragment, Git config override or refspec from the request.
    repo, ref, leaf = request["repo"], request["reference"], request["leaf"]
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9-]{0,38}/[A-Za-z0-9_.-]{1,100}", repo):
        raise ValueError("invalid repository")
    if repo.split("/")[1] in (".", ".."):
        raise ValueError("invalid repository")
    valid_ref = re.fullmatch(r"HEAD|[a-fA-F0-9]{40,64}|refs/heads/[A-Za-z0-9][A-Za-z0-9._/-]*|refs/pull/[1-9][0-9]*/head", ref)
    if not valid_ref or ".." in ref or "//" in ref or ref.endswith(("/", ".", ".lock")):
        raise ValueError("invalid ref")
    if not re.fullmatch(r"\.fridica-fetch-[A-Za-z0-9_-]{1,100}", leaf):
        raise ValueError("invalid destination")
    workspace = request["workspace"]
    if not (workspace.startswith("/") or workspace.startswith("~/")) or ".." in workspace.split("/"):
        raise ValueError("invalid workspace")
    executable = request["git"]
    if not executable.startswith("/") or "\x00" in executable:
        raise ValueError("invalid Git executable")
    for number in STOP_SIGNALS:
        signal.signal(number, interrupt)
    threading.Thread(target=watch_input, daemon=True).start()
    root = directory(workspace, request["create"])
    try:
        # Check before network work and again exclusively at publication.
        try:
            os.stat(leaf, dir_fd=root, follow_symlinks=False)
        except FileNotFoundError:
            pass
        else:
            raise FileExistsError("destination exists")
        with tempfile.TemporaryDirectory(prefix="fridica-fetch-", dir="/tmp") as stage:
            os.chmod(stage, 0o700)
            options = [executable, "-c", "core.hooksPath=/dev/null", "-c", "credential.helper=",
                       "-c", "protocol.allow=never", "-c", "protocol.https.allow=always",
                       "-c", "http.followRedirects=false", "-c", "maintenance.auto=false",
                       "-c", "gc.auto=0"]
            git(options + ["init", "--bare", "--template=", "-q", "repo"], stage, 30)
            bare = os.path.join(stage, "repo")
            git(options + ["fetch", "--quiet", "--no-tags", "--no-recurse-submodules",
                           "https://github.com/" + repo + ".git", ref], bare, request["timeout"])
            commit = git(options + ["rev-parse", "--verify", "FETCH_HEAD^{commit}"], bare, 30).decode("ascii").strip()
            if not re.fullmatch(r"[a-fA-F0-9]{40,64}", commit):
                raise RuntimeError("invalid fetched commit")
            os.mkdir(leaf, 0o700, dir_fd=root)
            target = os.open(leaf, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=root)
            try:
                publish(bare, target)
            finally:
                os.close(target)
            print(json.dumps({"path": workspace.rstrip("/") + "/" + leaf, "commit": commit}), flush=True)
    finally:
        os.close(root)


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # Arguments, Git stderr, paths, environment and repository contents never
        # become diagnostics. Rust records a fixed failure code and durable intent.
        sys.exit(1)
