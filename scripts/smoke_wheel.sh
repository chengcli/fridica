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
requests = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "wheel-smoke", "version": "1"}}},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "native_list_comments", "arguments": {}}},
]
result = subprocess.run([str(prefix / "bin/fridica"), "overleaf-mcp", "--workspace-write", str(workspace),
                         "--native-worker", worker, "--native-channel", channel, "--thread-project", "3" * 24,
                         "--native-session-stdin"],
                        input=json.dumps(grant) + "\n" + "".join(json.dumps(r) + "\n" for r in requests),
                        env=env, cwd=workspace,
                        text=True, stdout=subprocess.PIPE, timeout=30, check=True)
replies = [json.loads(line) for line in result.stdout.splitlines()]
listed = replies[1]["result"]["tools"]
names = {t["name"] for t in listed}
assert names == {"native_list_comments", "native_read_comment_thread", "native_add_comment", "native_reply_comment",
                 "native_read_file", "native_list_tracked_changes", "native_suggest_edit", "native_compile"}, names
for tool in listed:
    assert "project_id" in tool["inputSchema"]["required"]
    assert "default" not in tool["inputSchema"]["properties"]["project_id"]
refusal = replies[2]
assert refusal.get("error") or refusal.get("result", {}).get("isError") is True, refusal
assert "project" in json.dumps(refusal).lower() or "grant" in json.dumps(refusal).lower(), refusal
assert not list(workspace.glob(".overleaf-mcp-*")), "obsolete adapter scratch created"
print("installed Rust native preparation and launch passed without Node, Python helpers or a Git token; missing project refused")
PYCODE
echo "wheel smoke test passed"
