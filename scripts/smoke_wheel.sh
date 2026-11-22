#!/usr/bin/env bash
# Install one fridica wheel into a clean virtualenv and exercise its executables.
# Usage: scripts/smoke_wheel.sh WHEEL [EXPECTED_VERSION]
set -euo pipefail
wheel=$1
expected=${2:-}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
python -m venv "$work/venv"
"$work/venv/bin/python" -m pip install --quiet --no-deps "$wheel"
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
"$bin/fridica" alphaxiv-mcp --help > /dev/null
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
echo "wheel smoke test passed"
