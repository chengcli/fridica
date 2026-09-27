"""Verify frozen source/configuration/assets and the regression retention map."""
from pathlib import Path
import ast
import hashlib
import json


def main():
    root = Path(__file__).resolve().parents[1]
    frozen = root / "spec"
    manifest = json.loads((frozen / "baseline.json").read_text())
    for relative, expected in manifest["files"].items():
        path = frozen / relative
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        if actual != expected:
            raise SystemExit(f"frozen baseline changed: {relative}")
    mapping = json.loads((root / "tests/regression-map.json").read_text())
    tests = mapping["tests"]
    assert len(tests) == len({entry["python"] for entry in tests}) == mapping["baseline_tests"] == 389
    assert mapping["baseline_tag"] == manifest["tag"] == "v0.3.11"
    for entry in tests:
        assert entry["status"] in ("retained", "replaced")
        assert entry.get("reason") if entry["status"] == "retained" else entry.get("rust")
        if entry.get("related_rust_coverage"):
            assert (root / entry["related_rust_coverage"]).is_file()
    mapped = {(entry["python"].split("::")[0], entry["python"].split("::")[1].split("[")[0]) for entry in tests}
    actual = set()
    for path in (frozen / "tests").glob("test_*.py"):
        for node in ast.parse(path.read_text()).body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                actual.add((str(path.relative_to(frozen)), node.name))
    assert actual == mapped, "regression inventory differs from frozen tests"
    print(f"Verified {len(manifest['files'])} frozen files and {len(tests)} mapped regressions.")


if __name__ == "__main__":
    main()
