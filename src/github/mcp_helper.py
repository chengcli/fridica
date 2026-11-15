"""Target-local launcher/probe for the official GitHub MCP server.

Only names are supplied on argv. Neither credentials nor upstream diagnostics
are printed. Probe mode only initializes, lists tools and reads identity.
"""
import json
import os
import selectors
import shutil
import subprocess
import sys
import time

READ = ["get_me", "get_file_contents", "get_commit", "list_branches",
        "list_pull_requests", "search_pull_requests", "pull_request_read", "issue_read"]
WRITE = ["add_issue_comment", "add_comment_to_pending_review",
         "add_reply_to_pull_request_comment", "pull_request_review_write",
         "create_branch", "create_or_update_file", "delete_file", "push_files",
         "create_pull_request", "update_pull_request"]


def setup(request):
    command = shutil.which(os.path.expanduser(request["command"]))
    if not command:
        raise ValueError("unavailable")
    # The host, tool list and credential source cannot be changed by ambient
    # GITHUB_* variables or server config in a project's working directory.
    env = {k: v for k, v in os.environ.items()
           if not k.upper().startswith(("GITHUB_", "FRIDICA_"))
           and "SLACK" not in k.upper()}
    token = env.pop(request["token_env"], "") or os.environ.get(request["token_env"], "")
    if not token:
        auth = subprocess.run(["gh", "auth", "token", "--hostname", "github.com"],
                              env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, timeout=10, check=True)
        token = auth.stdout.decode().strip()
    if not token or any(c in token for c in "\r\n\0"):
        raise ValueError("authentication")
    env.pop("GH_TOKEN", None)
    env.pop("GITHUB_TOKEN", None)
    env["GITHUB_PERSONAL_ACCESS_TOKEN"] = token
    env["GITHUB_HOST"] = "https://github.com"
    tools = READ if request["readonly"] else READ + WRITE
    args = [command, "stdio", "--tools", ",".join(tools)]
    if request["readonly"]:
        args += ["--read-only"]
    return args, env, tools


def probe(args, env, required):
    child = subprocess.Popen(args, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                             stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 15
    selector = selectors.DefaultSelector()
    selector.register(child.stdout, selectors.EVENT_READ)
    buffer = b""

    def send(value):
        child.stdin.write(json.dumps(value).encode() + b"\n")
        child.stdin.flush()

    def response(identifier):
        nonlocal buffer
        while time.monotonic() < deadline:
            if b"\n" in buffer:
                line, buffer = buffer.split(b"\n", 1)
                value = json.loads(line)
                if value.get("id") == identifier:
                    if "error" in value:
                        raise ValueError("protocol")
                    return value["result"]
                # Notifications are permitted; unexpected server requests are not.
                if "id" in value:
                    raise ValueError("protocol")
                continue
            if not selector.select(max(0, deadline - time.monotonic())):
                break
            part = os.read(child.stdout.fileno(), 65536)
            if not part:
                raise ValueError("eof")
            buffer += part
            if len(buffer) > 2 * 1024 * 1024:
                raise ValueError("limit")
        raise ValueError("timeout")

    try:
        send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "fridica-readiness", "version": "1"}}})
        initialized = response(1)
        if "tools" not in initialized.get("capabilities", {}):
            raise ValueError("tools")
        send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        names, cursor = set(), None
        for page in range(64):
            identifier = page + 2
            send({"jsonrpc": "2.0", "id": identifier, "method": "tools/list",
                  "params": {} if cursor is None else {"cursor": cursor}})
            result = response(identifier)
            names.update(t["name"] for t in result["tools"])
            cursor = result.get("nextCursor")
            if not cursor:
                break
        else:
            raise ValueError("pagination")
        if names != set(required):
            raise ValueError("tool selection")
        send({"jsonrpc": "2.0", "id": 100, "method": "tools/call",
              "params": {"name": "get_me", "arguments": {}}})
        if response(100).get("isError"):
            raise ValueError("authentication")
        print("fridica-github-mcp:ready", flush=True)
    finally:
        selector.close()
        child.terminate()
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


def main():
    request = json.loads(sys.argv[1])
    args, env, tools = setup(request)
    # Avoid project-local server description overrides and inherited cwd effects.
    os.chdir("/")
    if sys.argv[2:] == ["--probe"]:
        probe(args, env, tools)
    else:
        # Do not expose the official server's startup diagnostics (which could
        # echo authentication details) to backend logs.
        with open(os.devnull, "wb") as sink:
            os.dup2(sink.fileno(), 2)
        os.execve(args[0], args, env)


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, KeyError, TypeError, AttributeError, IndexError,
            RecursionError, subprocess.SubprocessError):
        sys.stderr.write("fridica GitHub MCP: setup or probe failed\n")
        sys.exit(97)
