#!/usr/bin/env python3
"""MCP policy/composition adapter; no Overleaf editing/comment protocol here.

The pinned AGPL package runs in its own process. Only its MCP interface is
used. Linux memfd supplies its cookie *store* without a secret-bearing disk file.
"""
import argparse
import hashlib
import html.parser
import json
import os
from pathlib import Path
import queue
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
import uuid

ORIGIN = "https://www.overleaf.com"
OPERATIONS = ["list_comments", "read_comment_thread", "add_comment", "reply_comment",
              "read_file", "list_tracked_changes", "find_and_replace"]
NAMES = {"native_" + name: name for name in OPERATIONS}
NAMES["native_suggest_edit"] = NAMES.pop("native_find_and_replace")
WRITES = {"add_comment", "reply_comment", "find_and_replace"}
PIN = "0.4.0"
SOURCE = "e4ad4f97d5730a448e5a2c3aa60838ef2d1f8981"
LIMIT = 8 * 1024 * 1024


class Refusal(ValueError):
    """Only these static policy messages may reach the MCP client."""


def require(ok, message="Native operation refused by scope policy"):
    if not ok:
        raise Refusal(message)


def session(raw, worker, channel):
    require(len(raw) <= 16384)
    s = json.loads(raw)
    require(isinstance(s, dict) and s.get("schema") == 2)
    require(isinstance(s.get("projects"), list) and len(s["projects"]) == 1 and re.fullmatch(r"[0-9a-f]{24}", s["projects"][0]) and s.get("operations") == OPERATIONS)
    require(s.get("channel") == channel and re.fullmatch(r"[CG][A-Z0-9]+", channel))
    r = s.get("recipient", {})
    require(r.get("worker_id") == worker and all(isinstance(r.get(k), str) and r[k] and not any(ord(c) < 32 for c in r[k]) for k in ("machine", "workspace")))
    require(re.fullmatch(r"worker-[0-9a-f-]{36}", worker))
    for deadline in (s.get("expires_at"), r.get("expires_at")):
        require(deadline is None or type(deadline) is int and deadline > time.time(), "Native session expired; owner login/rebinding required")
    require(re.fullmatch(r"[0-9a-f]{24}", s.get("account_id", "")))
    require(re.fullmatch(r"[^\s@]+@[^\s@]+", s.get("expected_email", "")))
    pairs = s.get("cookie", "").split("; ")
    names = set()
    for p in pairs:
        k, sep, v = p.partition("=")
        require(sep and k in {"overleaf_session2", "GCLB"} and k not in names)
        require(v and len(v) <= 4096 and all(33 <= ord(c) < 127 and c not in ';"\\,' for c in v))
        names.add(k)
    require("overleaf_session2" in names)
    return s


class Meta(html.parser.HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.values = {}

    def handle_starttag(self, tag, attrs):
        d = dict(attrs)
        if tag == "meta" and d.get("name") in {"ol-user", "ol-project_id"}:
            require(d["name"] not in self.values, "Ambiguous account/project metadata")
            self.values[d["name"]] = d.get("content", "")


def identity(body, s):
    m = Meta()
    m.feed(body)
    require(m.values.get("ol-project_id") == s["projects"][0], "Native project verification failed")
    user = json.loads(m.values.get("ol-user", "{}"))
    require(user.get("_id", user.get("id")) == s["account_id"]
            and user.get("email", "").lower() == s["expected_email"].lower(), "Native account verification failed")


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def verify_identity(s):
    # Independent, fixed HTTPS account/project check. No dashboard listing or redirects.
    request = urllib.request.Request(f"{ORIGIN}/project/{s['projects'][0]}", headers={"Cookie": s["cookie"], "Accept": "text/html"})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        with opener.open(request, timeout=15) as response:
            require(response.status == 200 and response.headers.get_content_type() == "text/html")
            body = response.read(1024 * 1024 + 1)
            require(len(body) <= 1024 * 1024)
        identity(body.decode("utf-8"), s)
    except Exception:
        raise Refusal("Native account/project verification failed; check owner login or session expiry") from None


class Peer:
    """Only newline-delimited MCP JSON-RPC; all native protocol belongs upstream."""
    def __init__(self, argv, env, cwd, fds=()):
        self.process = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL, env=env, cwd=cwd, pass_fds=fds)
        self.responses = queue.Queue()
        self.next_id = 0
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        try:
            self.info = self.call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                                "clientInfo": {"name": "fridica", "version": "0.1.0"}})
            self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        except BaseException:
            self.close()
            raise

    def read(self):
        try:
            while True:
                line = self.process.stdout.readline(LIMIT + 1)
                if not line or len(line) > LIMIT:
                    break
                value = json.loads(line)
                if "id" in value:
                    self.responses.put(value)
        except Exception:
            pass
        self.responses.put(None)

    def send(self, value):
        self.process.stdin.write(json.dumps(value, separators=(",", ":")).encode() + b"\n")
        self.process.stdin.flush()

    def call(self, method, params=None):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}})
        response = self.responses.get(timeout=90)
        require(response is not None and response.get("id") == self.next_id and "error" not in response,
                "MCP subprocess failed; reconcile any submitted write before retrying")
        return response["result"]

    def close(self):
        self.process.terminate()
        try:
            self.process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
        self.reader.join(timeout=1)
        self.process.stdin.close()
        self.process.stdout.close()


def native_peer(node, root, state, cookie=""):
    require(sys.platform == "linux" and hasattr(os, "memfd_create"), "Native worker requires Linux memfd")
    pkg = root / "node_modules/@netique/overleaf-mcp"
    require(json.loads((pkg / "package.json").read_text())["version"] == PIN, "Upstream package pin mismatch")
    installed = json.loads((root / "node_modules/.package-lock.json").read_text())
    require(installed["packages"]["node_modules/@netique/overleaf-mcp"]["resolved"].endswith("#" + SOURCE), "Upstream source pin mismatch")
    store = state / "overleaf-mcp"
    store.mkdir(mode=0o700)
    fd = os.memfd_create("fridica-overleaf-session", os.MFD_CLOEXEC | os.MFD_ALLOW_SEALING)
    try:
        import fcntl
        os.fchmod(fd, 0o600)
        os.write(fd, json.dumps({"hosts": {"www.overleaf.com": {"cookie": cookie, "savedAt": int(time.time() * 1000)}}}).encode())
        os.lseek(fd, 0, os.SEEK_SET)
        fcntl.fcntl(fd, fcntl.F_ADD_SEALS, fcntl.F_SEAL_WRITE | fcntl.F_SEAL_GROW | fcntl.F_SEAL_SHRINK | fcntl.F_SEAL_SEAL)
        (store / "cookie.json").symlink_to(f"/proc/self/fd/{fd}")
        # No inherited credentials, proxy settings, NODE_OPTIONS, browser/profile config.
        # A nonexistent explicit browser path disables fallback discovery/re-login/retry.
        env = {"PATH": "/usr/bin:/bin", "XDG_CONFIG_HOME": str(state), "OL_BASE_URL": ORIGIN,
               "OL_BROWSER": str(state / "login-disabled"), "OL_MCP_LOG_LEVEL": "error"}
        return Peer([node, str(pkg / "dist/index.js")], env, str(state), (fd,))
    finally:
        os.close(fd)


def arguments(name, given, schema, project):
    require(name in NAMES and isinstance(given, dict))
    op = NAMES[name]
    args = dict(given)
    opid = args.pop("operation_id", None)
    require(set(args) <= set(schema["properties"]))
    require(args.get("project_id", project) == project)
    args["project_id"] = project
    if "thread_id" in args:
        require(isinstance(args["thread_id"], str) and re.fullmatch(r"[0-9a-f]{24}", args["thread_id"]))
    if "path" in args:
        require(isinstance(args["path"], str) and args["path"] and not args["path"].startswith("/")
                and ".." not in args["path"].split("/") and "\\" not in args["path"])
    if op in WRITES:
        require(isinstance(opid, str) and str(uuid.UUID(opid)) == opid, "A stable operation_id UUID is required for writes")
    if op == "find_and_replace":
        require(args.get("track", "on") == "on" and args.get("strict_version", True) is True)
        require(type(args.get("expected_version")) is int and args["expected_version"] >= 0, "Read the current native version before suggesting an edit")
        args.update(track="on", strict_version=True)
    return op, args, opid


def write_once(root, op, args, opid, call):
    # Only hashes, state and safe identifiers/counts persist. Never manuscript/comment text.
    digest = hashlib.sha256(json.dumps([op, args], sort_keys=True).encode()).hexdigest()
    receipt = root / (hashlib.sha256(opid.encode()).hexdigest() + ".json")
    try:
        fd = os.open(receipt, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    except FileExistsError:
        prior = json.loads(receipt.read_text())
        require(prior.get("digest") == digest, "operation_id already used for another payload")
        require(prior.get("state") == "confirmed", "Previous write outcome uncertain; read the thread and reconcile, do not replay")
        return {"content": [{"type": "text", "text": "Already submitted; no replay."}], "structuredContent": prior["receipt"]}
    with os.fdopen(fd, "w") as stream:
        json.dump({"digest": digest, "state": "uncertain"}, stream)
        stream.flush()
        os.fsync(stream.fileno())
    for directory in (root.parent, root):
        directory_fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory_fd)
        finally:
            os.close(directory_fd)
    result = call()
    if not result.get("isError"):
        data = result.get("structuredContent", {})
        safe = {}
        if isinstance(data.get("thread_id"), str) and re.fullmatch(r"[0-9a-f]{24}", data["thread_id"]):
            safe["thread_id"] = data["thread_id"]
        if type(data.get("content_length")) is int and data["content_length"] >= 0:
            safe["content_length"] = data["content_length"]
        tmp = receipt.with_suffix(".tmp")
        with open(tmp, "x", opener=lambda p, f: os.open(p, f, 0o600)) as stream:
            json.dump({"digest": digest, "state": "confirmed", "receipt": safe}, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, receipt)
    return result


class Adapter:
    def __init__(self, options):
        self.options = options
        self.root = Path(__file__).resolve().parent
        self.workspace = Path(options.workspace).resolve(strict=True)
        require(self.workspace.is_dir())
        self.raw = os.environ.pop("OVERLEAF_NATIVE_SESSION", "")
        self.grant = session(self.raw, options.worker, options.channel) if self.raw else None
        self.state = tempfile.TemporaryDirectory(prefix=".overleaf-mcp-", dir=self.workspace)
        self.native = None
        self.builtin = None
        try:
            safe_env = {k: v for k, v in os.environ.items() if k in {
                "PATH", "HOME", "LANG", "TMPDIR", "SSL_CERT_FILE", "SSL_CERT_DIR", "OVERLEAF_GIT_TOKEN",
                "ADS_TOKEN", "OVERLEAF_PROJECT_ID", "INTERLEAF_CONFIG", "INTERLEAF_WORKSPACE"}}
            self.builtin = Peer([options.builtin, "overleaf-mcp", "--workspace-write", str(self.workspace)], safe_env, str(self.workspace))
            self.native = native_peer(options.node, self.root, Path(self.state.name), self.grant["cookie"] if self.grant else "")
            tools = self.native.call("tools/list")["tools"]
            self.schemas = {t["name"]: t for t in tools if t["name"] in OPERATIONS}
            require(set(self.schemas) == set(OPERATIONS), "Required upstream tools are missing")
        except BaseException:
            self.close()
            raise

    def close(self):
        for peer in (self.native, self.builtin):
            if peer:
                peer.close()
        self.state.cleanup()

    def dispatch(self, method, params):
        if method == "initialize":
            return {**self.builtin.info, "instructions": "Use built-in Interleaf for Git/compile. Native review tools are restricted to the owner-configured project. Native suggestions must remain pending; never accept/reject author changes."}
        if method == "tools/list":
            tools = self.builtin.call(method, params)["tools"]
            for name, op in (NAMES.items() if self.grant else []):
                tool = json.loads(json.dumps(self.schemas[op]))
                tool["name"] = name
                tool["inputSchema"]["properties"]["project_id"] = {"type": "string", "const": self.grant["projects"][0], "default": self.grant["projects"][0]}
                if op in WRITES:
                    tool["inputSchema"]["properties"]["operation_id"] = {"type": "string", "description": "Stable UUID; reuse after ambiguous outcomes to prevent replay."}
                    tool["inputSchema"].setdefault("required", []).append("operation_id")
                if op == "find_and_replace":
                    tool["inputSchema"]["properties"]["track"] = {"const": "on", "type": "string"}
                    tool["inputSchema"]["properties"]["strict_version"] = {"const": True, "type": "boolean"}
                    tool["inputSchema"].setdefault("required", []).append("expected_version")
                tools.append(tool)
            return {"tools": tools}
        if method != "tools/call" or not params.get("name", "").startswith("native_"):
            return self.builtin.call(method, params)
        name = params.get("name")
        require(name in NAMES)
        require(self.grant is not None, "Native session not configured; owner login and explicit recipient binding required")
        op, args, opid = arguments(name, params.get("arguments", {}), self.schemas[NAMES[name]]["inputSchema"], self.grant["projects"][0])
        s = session(self.raw, self.options.worker, self.options.channel)
        verify_identity(s)
        def call():
            result = self.native.call("tools/call", {"name": op, "arguments": args})
            if result.get("isError"):
                return {"isError": True, "content": [{"type": "text", "text": "Native operation failed; reconcile submitted writes by reading the thread. No adapter retry was attempted."}]}
            encoded = json.dumps(result)
            for part in s["cookie"].split("; "):
                encoded = encoded.replace(part.split("=", 1)[1], "[redacted]")
            return json.loads(encoded)
        if op in WRITES:
            receipts = self.workspace / ".fridica-overleaf-receipts"
            receipts.mkdir(mode=0o700, exist_ok=True)
            return write_once(receipts, op, args, opid, call)
        return call()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--node", required=True)
    p.add_argument("--builtin", required=True)
    p.add_argument("--workspace", required=True)
    p.add_argument("--worker", required=True)
    p.add_argument("--channel", required=True)
    o = p.parse_args()
    adapter = None
    def stop(*_):
        raise SystemExit(0)
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        adapter = Adapter(o)
        while True:
            line = sys.stdin.buffer.readline(LIMIT + 1)
            if not line:
                break
            require(len(line) <= LIMIT)
            request = json.loads(line)
            if "id" not in request:
                continue
            try:
                result = adapter.dispatch(request["method"], request.get("params", {}))
                response = {"jsonrpc": "2.0", "id": request["id"], "result": result}
            except Exception as error:
                # Never echo exceptions from subprocesses, HTTPS, JSON, or payload validation.
                message = str(error) if isinstance(error, Refusal) else "MCP operation failed; reconcile any submitted write before retrying"
                response = {"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32000, "message": message}}
            print(json.dumps(response), flush=True)
    except Exception:
        print("Scoped Overleaf adapter startup/transport failed", file=sys.stderr)
        return 1
    finally:
        if adapter:
            adapter.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
