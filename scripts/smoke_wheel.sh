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
version=$("$bin/fridica" --version)
echo "$version"
[[ $version == "fridica "* ]]
if [[ -n $expected ]]; then
  [[ $version == "fridica $expected" ]] || { echo "expected fridica $expected" >&2; exit 1; }
fi
"$bin/fridica-overseer" --version
"$bin/fridica" build-info
[[ $("$bin/fridica" assets --list | wc -l) -gt 0 ]]
# A fresh configuration is created without network, credentials or a source checkout.
cd "$work"
HOME="$work" "$bin/fridica" init --config "$work/config/config.toml" > /dev/null
test -s "$work/config/config.toml" && test -s "$work/config/contract.md"
echo "wheel smoke test passed"
