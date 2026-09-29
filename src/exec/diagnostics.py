"""Fixed non-model doctor probes. Only a fixed result code leaves this helper."""
import json
import os
import selectors
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

LIMIT = 4 * 1024 * 1024


class Refused(Exception):
    pass


def stop(_signum, _frame):
    raise Refused()


for signum in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(signum, stop)


def run(argv):
    # Separate child group lets normal completion also remove surviving tools.
    child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                             stderr=subprocess.PIPE, start_new_session=True)
    output = bytearray()
    total = 0
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(child.stdout, selectors.EVENT_READ, True)
            selector.register(child.stderr, selectors.EVENT_READ, False)
            selector.register(sys.stdin, selectors.EVENT_READ, None)
            streams = 2
            while streams:
                if time.monotonic() >= deadline:
                    raise Refused()
                for key, _ in selector.select(min(0.1, max(0, deadline - time.monotonic()))):
                    data = os.read(key.fd, 65536)
                    if key.data is None:
                        # Parent exit or remote disconnect must terminate diagnostics.
                        raise Refused()
                    if not data:
                        selector.unregister(key.fileobj)
                        streams -= 1
                    else:
                        total += len(data)
                        if total > LIMIT:
                            raise Refused()
                        if key.data:
                            output.extend(data)
            code = child.wait(timeout=max(0.001, deadline - time.monotonic()))
        return code, output.decode("utf-8", errors="replace")
    finally:
        try:
            os.killpg(child.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        child.wait(timeout=1)
        child.stdout.close()
        child.stderr.close()


def schema_text(directory):
    # Do not follow generated symlinks or read devices/FIFOs. Bound disk input.
    parts = []
    size = 0
    entries = 0
    for root, dirs, files in os.walk(directory, followlinks=False):
        entries += len(dirs) + len(files)
        if entries > 4096 or time.monotonic() >= deadline:
            raise Refused()
        for name in files:
            fd = os.open(os.path.join(root, name), os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, "rb") as stream:
                if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                    raise Refused()
                data = stream.read(LIMIT - size + 1)
            size += len(data)
            if size > LIMIT:
                raise Refused()
            parts.append(data.decode("utf-8", errors="replace"))
    return "\n".join(parts)


def check(request):
    action = request["action"]
    backend = request["backend"]
    if action == "system":
        return {"linux": "linux", "darwin": "darwin"}.get(sys.platform, "failed")
    if action == "workspace":
        workspace = request["workspace"]
        if workspace.startswith("~/"):
            workspace = os.path.join(os.environ["HOME"], workspace[2:])
        return "passed" if os.path.isdir(workspace) else "failed"
    if action == "sandbox":
        if not shutil.which("bwrap") or (backend == "claude" and not shutil.which("socat")):
            return "missing"
        code, _ = run(["bwrap", "--unshare-user", "--unshare-net", "--ro-bind", "/", "/",
                       "--dev", "/dev", "--proc", "/proc", "--die-with-parent", "--", "/bin/true"])
        return "passed" if code == 0 else "failed"
    if backend not in ("claude", "codex"):
        raise Refused()
    if not shutil.which(backend):
        return "missing"
    if action == "auth":
        code, text = run([backend, "auth", "status"] if backend == "claude"
                         else [backend, "login", "status"])
        if code != 0:
            return "signed_out"
        if backend == "claude":
            try:
                value = json.loads(text)
            except ValueError:
                return "signed_out"
            if not isinstance(value, dict) or value.get("loggedIn") is not True:
                return "signed_out"
        return "passed"
    if action != "capabilities":
        raise Refused()
    if backend == "claude":
        code, text = run([backend, "--help"])
        required = ["--json-schema", "--setting-sources", "--strict-mcp-config"]
        required += (["dontAsk"] if request["parent"] else
                     ["--input-format", "--permission-prompts", "--append-system-prompt", "--session-id"])
        auto = '"auto"'
    elif request["parent"]:
        code, text = run([backend, "exec", "--help"])
        required = ["--ignore-user-config", "--ignore-rules", "--output-schema", "--ephemeral"]
        auto = "auto_review"
    else:
        with tempfile.TemporaryDirectory(prefix="fridica-doctor-") as directory:
            code, _ = run([backend, "app-server", "generate-json-schema", "--out", directory])
            text = schema_text(directory) if code == 0 else ""
        required = ["turn/interrupt", "item/commandExecution/requestApproval", "outputSchema"]
        auto = "auto_review"
    if code != 0 or any(flag not in text for flag in required):
        return "protocol_missing"
    if request["auto"] and auto not in text:
        return "auto_missing"
    return "passed"


try:
    request = json.loads(sys.argv[1])
    deadline = time.monotonic() + request["timeout"]
    # Also scrub the inherited environment on the SSH target itself.
    for name, value in list(os.environ.items()):
        if (name in request["excluded"] or "SLACK" in name.upper()
                or name.upper().startswith("FRIDICA_")
                or value.startswith(("xoxp-", "xoxb-", "xapp-", "xoxe-"))):
            del os.environ[name]
    if request["home"] is not None:
        os.environ["HOME"] = request["home"]
    result = check(request)
except (Exception, KeyboardInterrupt):
    result = "failed"
print("fridica-doctor:" + result, flush=True)
