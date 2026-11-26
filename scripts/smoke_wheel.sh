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
# Only the installed prefix and ordinary Node are needed for owner preparation.
# No explicit --resources, npm/compiler/browser/model client, credentials or auth.
node_bin=$(command -v node)
"$bin/python" - "$work" "$node_bin" <<'PYCODE'
import hashlib, json, pathlib, shlex, shutil, subprocess, sys, tarfile, tomllib
work = pathlib.Path(sys.argv[1])
prefix = work / "venv"
resources = prefix / "share/fridica/overleaf-native"
upstream = resources / "node_modules/@netique/overleaf-mcp"
for name in ("adapter.py", "owner-login.mjs", "FRIDICA-LICENSE", "UPSTREAM-NOTICE"):
    assert (resources / name).is_file(), name
assert "GNU AFFERO GENERAL PUBLIC LICENSE" in (upstream / "LICENSE").read_text()
assert "MIT License" in (resources / "FRIDICA-LICENSE").read_text()
pin = json.loads((resources / "upstream-pin.json").read_text())
lock = json.loads((resources / "package-lock.json").read_text())
expected = lock["packages"]["node_modules/@netique/overleaf-mcp"]
assert pin["resolved"] == expected["resolved"]
assert pin["resolved"].endswith("#42135403af5e76b80348303d742a5e1fe4da6d30")
source = resources / "upstream-source.tar.gz"
assert hashlib.sha256(source.read_bytes()).hexdigest() == pin["corresponding_source_sha256"]
with tarfile.open(source) as archive:
    for name in ("LICENSE", "package.json", "package-lock.json", "src/api/socket.ts", "src/auth/sessionFd.ts"):
        assert archive.getmember("overleaf-mcp/" + name).isfile(), name
assert not list(resources.rglob("*.node")), "platform-specific Node addon in wheel"
tools = work / "runtime-bin"
tools.mkdir()
# Copy Node so neither its install directory nor a source checkout is on PATH.
shutil.copy2(sys.argv[2], tools / "node")
env = {"PATH": str(tools), "HOME": str(work), "TMPDIR": str(work),
       "XDG_CONFIG_HOME": str(work / "xdg-config"), "XDG_DATA_HOME": str(work / "xdg-data")}
for name in ("npm", "cargo", "rustc", "cc", "chromium", "codex", "claude"):
    assert shutil.which(name, path=env["PATH"]) is None
config = work / "owner/config.toml"
subprocess.run([str(prefix / "bin/fridica"), "overleaf-auth", "login", "--prepare-only",
                "--config", str(config), "--expected-email", "owner@example.invalid"],
               cwd=work, env=env, check=True, timeout=30)
overleaf = tomllib.loads(config.read_text())["overleaf"]
assert pathlib.Path(overleaf["login"]["resources"]).resolve() == resources.resolve()
assert "native_recipient" not in overleaf and "native_adapter" not in overleaf
assert overleaf["enabled"] is True
assert overleaf["native_scope"] == {"expected_email": "owner@example.invalid"}
helpers = list((config.parent / "overleaf-login").glob("*/node_modules"))
assert len(helpers) == 1 and helpers[0].resolve() == (resources / "node_modules").resolve()
# Exercise the installed worker entry and real adapter/upstream/built-in processes,
# without Git tokens, owner stores, model backends, browser or authenticated calls.
# Worker launches inherit the execution target's activated Python environment.
# Keep owner preparation above independent of Python on PATH, then instrument the
# selected venv interpreter to catch an adapter that bypasses that environment.
python_bin = work / "selected-python"
python_bin.mkdir()
python_marker = work / "selected-python.log"
python_shim = python_bin / "python3"
python_shim.write_text("#!/bin/sh\nprintf 'selected\\n' >> " + shlex.quote(str(python_marker))
                       + "\nexec " + shlex.quote(str(prefix / "bin/python3")) + ' "$@"\n')
python_shim.chmod(0o700)
env = {**env, "PATH": str(python_bin) + ":" + str(prefix / "bin") + ":" + env["PATH"],
       "VIRTUAL_ENV": str(prefix)}
workspace = work / "worker"
workspace.mkdir()
probe = subprocess.run([str(prefix / "bin/fridica"), "overleaf-mcp", "--workspace-write", str(workspace),
                        "--native-probe", "--probe"], env=env, cwd=workspace,
                       text=True, capture_output=True, timeout=30, check=True)
assert probe.stdout == "fridica-overleaf-mcp:native-ready\n"
worker = "worker-11111111-1111-4111-8111-111111111111"
channel = "CEXAMPLE"
operations = ["list_comments", "read_comment_thread", "add_comment", "reply_comment",
              "read_file", "list_tracked_changes", "find_and_replace"]
grant = dict(schema=3, cookie="overleaf_session2=synthetic-wheel-fixture", account_id="1" * 24,
             expected_email="owner@example.invalid", operations=operations, channel=channel,
             recipient=dict(worker_id=worker, machine="fixture", workspace="paper"))
requests = [
    {"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "wheel-smoke", "version": "1"}}},
    {"jsonrpc": "2.0", "id": 2, "method": "tools/list"},
    {"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "native_list_comments", "arguments": {}}},
]
result = subprocess.run([str(prefix / "bin/fridica"), "overleaf-mcp", "--workspace-write", str(workspace),
                         "--native-worker", worker, "--native-channel", channel],
                        input="".join(json.dumps(r) + "\n" for r in requests),
                        env={**env, "OVERLEAF_NATIVE_SESSION": json.dumps(grant)}, cwd=workspace,
                        text=True, capture_output=True, timeout=30, check=True)
replies = [json.loads(line) for line in result.stdout.splitlines()]
listed = replies[1]["result"]["tools"]
names = {t["name"] for t in listed}
assert {"native_list_comments", "native_read_comment_thread", "native_add_comment", "native_reply_comment",
        "native_read_file", "native_list_tracked_changes", "native_suggest_edit"} <= names
assert any(not name.startswith("native_") for name in names), "built-in tools missing"
for tool in listed:
    if tool["name"].startswith("native_"):
        assert "project_id" in tool["inputSchema"]["required"]
        assert "default" not in tool["inputSchema"]["properties"]["project_id"]
assert "explicit project_id" in replies[2]["error"]["message"]
assert not list(workspace.glob(".overleaf-mcp-*")), "adapter scratch not cleaned"
assert python_marker.is_file() and python_marker.read_text().splitlines() == ["selected", "selected"], \
    "installed native probe/worker bypassed the selected Python environment"
print("installed account login preparation and automatic native/built-in launch passed without a Git token; missing target refused")
PYCODE
echo "wheel smoke test passed"
