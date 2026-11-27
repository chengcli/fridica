#!/usr/bin/env bash
# Install one fridica wheel into a clean virtualenv and exercise its executables.
# Usage: scripts/smoke_wheel.sh WHEEL [EXPECTED_VERSION]
set -euo pipefail
wheel=$1
expected=${2:-}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python -m venv "$work/venv"
"$work/venv/bin/python" -m pip install --quiet --no-index --no-deps "$wheel"
bin="$work/venv/bin"
test -x "$bin/fridica-secret"
"$bin/fridica" secret --help > /dev/null
version=$("$bin/fridica" --version)
echo "$version"
[[ $version == "fridica "* ]]
if [[ -n $expected ]]; then
  [[ $version == "fridica $expected" ]] || { echo "expected fridica $expected" >&2; exit 1; }
fi
"$bin/fridica" build-info
for server in alphaxiv-mcp scix-mcp overleaf-mcp; do
  "$bin/fridica" "$server" --help > /dev/null
done
"$bin/python" - "$bin/fridica" <<'PY'
import json, subprocess, sys
requests = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2024-11-05"}},
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
]
result = subprocess.run([sys.argv[1], "github-mcp", "--read-only"],
                        input="".join(json.dumps(r) + "\n" for r in requests),
                        text=True, capture_output=True, check=True, timeout=15)
replies = [json.loads(line) for line in result.stdout.splitlines()]
names = {t["name"] for t in replies[-1]["result"]["tools"]}
assert {"get_me", "git_context", "pull_request_read"} <= names
assert "push_files" not in names
PY
[[ $("$bin/fridica" assets --list | wc -l) -gt 0 ]]
# A fresh configuration is created without network, credentials or a source checkout.
cd "$work"
HOME="$work" "$bin/fridica" init --config "$work/config/config.toml" > /dev/null
test -s "$work/config/config.toml" && test -s "$work/config/contract.md"
# The installed Rust backend prepares and lists native tools without Node,
# Python helpers, a browser, credentials, a compiler or a source checkout.
"$bin/python" - "$work" <<'PYCODE'
import json, pathlib, shutil, subprocess, sys, tomllib
work = pathlib.Path(sys.argv[1]).resolve()
prefix = work / "venv"
assert not (prefix / "share/fridica/overleaf-native").exists(), "obsolete native resources installed"
tools = work / "runtime-bin"
tools.mkdir()
env = {"PATH": str(tools), "HOME": str(work), "TMPDIR": str(work),
       "XDG_CONFIG_HOME": str(work / "xdg-config"), "XDG_DATA_HOME": str(work / "xdg-data")}
for name in ("node", "npm", "python", "python3", "cargo", "rustc", "cc", "chromium", "codex", "claude", "git"):
    assert shutil.which(name, path=env["PATH"]) is None
config = work / "owner/config.toml"
subprocess.run([str(prefix / "bin/fridica"), "overleaf-auth", "login", "--prepare-only",
                "--config", str(config), "--expected-email", "owner@example.invalid"],
               cwd=work, env=env, check=True, timeout=30)
overleaf = tomllib.loads(config.read_text())["overleaf"]
assert "native_recipient" not in overleaf and "native_adapter" not in overleaf
assert not ({"resources", "node"} & overleaf.get("login", {}).keys())
assert overleaf["enabled"] is True
assert overleaf["native_scope"] == {"expected_email": "owner@example.invalid"}
workspace = work / "worker"
workspace.mkdir()
probe = subprocess.run([str(prefix / "bin/fridica"), "overleaf-mcp", "--workspace-write", str(workspace),
                        "--native-probe", "--probe"], env=env, cwd=workspace,
                       text=True, stdout=subprocess.PIPE, timeout=30, check=True)
assert probe.stdout == "fridica-overleaf-mcp:native-ready\n"
worker = "worker-11111111-1111-4111-8111-111111111111"
channel = "CEXAMPLE"
operations = ["list_comments", "read_comment_thread", "add_comment", "reply_comment",
              "read_file", "list_tracked_changes", "find_and_replace"]
grant = dict(schema=3, cookie="overleaf_session2=synthetic-wheel-fixture", account_id="1" * 24,
             expected_email="owner@example.invalid", operations=operations, channel=channel, projects=["3" * 24],
             recipient=dict(worker_id=worker, machine="fixture", workspace="paper"))
def call(name, arguments):
    return {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": arguments}}

def invoke(grant, target, request):
    requests = [
        {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "wheel-smoke", "version": "1"}}},
        {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
        request,
    ]
    result = subprocess.run([str(prefix / "bin/fridica"), "overleaf-mcp", "--workspace-write", str(workspace),
                            "--native-worker", worker, "--native-channel", channel, "--thread-project", target,
                            "--native-session-stdin"],
                           input=json.dumps(grant) + "\n" + "".join(json.dumps(r) + "\n" for r in requests),
                           env=env, cwd=workspace, text=True, stdout=subprocess.PIPE, timeout=30, check=True)
    assert grant["cookie"] not in result.stdout
    replies = {reply["id"]: reply for line in result.stdout.splitlines() if (reply := json.loads(line)).get("id")}
    listed = replies[2]["result"]["tools"]
    for tool in listed:
        if tool["name"] not in {"native_ping", "native_list_projects", "native_create_project"}:
            assert "project_id" in tool["inputSchema"]["required"]
            assert "default" not in tool["inputSchema"]["properties"]["project_id"]
    return {t["name"] for t in listed}, replies[3]

def refused(reply):
    assert reply.get("error") or reply.get("result", {}).get("isError") is True, reply

helpers = {"native_suggest_edit", "native_compile_local"}
names, reply = invoke(grant, "3" * 24, call("native_list_comments", {}))
assert names == {"native_" + op for op in operations} | helpers, names
refused(reply)
assert "project" in json.dumps(reply).lower() or "grant" in json.dumps(reply).lower(), reply

canonical = set("""ping create_project list_projects open_project list_files read_file
edit_file find_and_replace create_doc create_folder upload_file rename_entity move_entity delete_entity
list_history diff_file list_tracked_changes accept_changes set_track_changes reject_changes
compile label_pages read_log list_comments read_comment_thread add_comment reply_comment
resolve_comment reopen_comment delete_comment edit_comment_message delete_comment_message search_project""".split())
assert len(canonical) == 33
full = {**grant, "schema": 4, "base_url": "http://127.0.0.1:9", "operations": sorted(canonical | {"direct_edit"})}
expected_tools = {"native_" + op for op in canonical} | helpers
names, reply = invoke(full, "3" * 24, call("native_ping", {}))
assert names == expected_tools, names ^ expected_tools
assert reply["result"]["structuredContent"]["pong"] is True, reply
_, reply = invoke(full, "3" * 24, call("native_read_file", {"project_id": "4" * 24}))
refused(reply)
assert "host-confirmed task project" in json.dumps(reply), reply
_, reply = invoke(full, "unbound", call("native_reply_comment", {
    "project_id": "3" * 24, "thread_id": "4" * 24, "content": "synthetic guard check",
    "operation_id": "11111111-1111-4111-8111-111111111111"}))
refused(reply)
assert "unbound native tasks may only read" in json.dumps(reply), reply
assert not list(workspace.glob(".overleaf-mcp-*")), "obsolete adapter scratch created"
print("installed Rust native legacy/full catalogs passed without Node, Python helpers or a Git token; missing/wrong project and unbound writes refused")
PYCODE
echo "wheel smoke test passed"
