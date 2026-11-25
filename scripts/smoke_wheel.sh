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
import hashlib, json, pathlib, shutil, subprocess, sys, tarfile, tomllib
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
                "--config", str(config), "--expected-email", "owner@example.invalid",
                "--project", "0123456789abcdef01234567"],
               cwd=work, env=env, check=True, timeout=30)
overleaf = tomllib.loads(config.read_text())["overleaf"]
assert pathlib.Path(overleaf["login"]["resources"]).resolve() == resources.resolve()
assert "native_recipient" not in overleaf
helpers = list((config.parent / "overleaf-login").glob("*/node_modules"))
assert len(helpers) == 1 and helpers[0].resolve() == (resources / "node_modules").resolve()
print("installed Overleaf resources, immutable pin, licenses/source and login preparation passed")
PYCODE
echo "wheel smoke test passed"
