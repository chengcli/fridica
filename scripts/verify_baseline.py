"""Verify frozen source/configuration/assets and the regression retention map."""
from pathlib import Path
import ast
import hashlib
import json
import re
from collections import Counter


def verify_audit(root, mapping):
    """Every retained case needs a reviewed rationale and resolvable evidence.

    Evidence is deliberately scoped: naming a Rust test does not retire its
    Python counterpart or certify complete differential parity.
    """
    groups = mapping["audit"]["groups"]
    used = set()
    counts = Counter()
    rust_inventory = {}
    for entry in mapping["tests"]:
        key = entry.get("audit_group")
        assert key in groups, f"missing audit group: {entry['python']}"
        group = groups[key]
        used.add(key)
        coverage = group["coverage"]
        assert coverage in ("scoped", "changed", "python-only"), key
        counts[coverage] += 1
        reason = group.get("retention_reason", "")
        assert len(reason.strip()) >= 80, f"missing specific retention reason: {key}"
        assert entry["reason"] == reason, f"stale retention reason: {entry['python']}"
        assert group.get("next_gate") in (
            "recovery-replay", "candidate-packaging", "deployment", "dashboard",
            "python-tooling", "out-of-scope",
        ), key
        evidence = group.get("rust_tests", [])
        assert isinstance(evidence, list) and len(evidence) == len(set(evidence)), key
        assert bool(evidence) == (coverage != "python-only"), f"missing or misleading Rust evidence: {key}"
        for reference in evidence:
            path, name = reference.split("::", 1)
            assert path.startswith("tests/") and path.endswith(".rs") and ".." not in Path(path).parts, reference
            if path not in rust_inventory:
                rust_inventory[path] = set(re.findall(
                    r"(?m)^#\[(?:tokio::)?test(?:\([^\n]*\))?\]\s*"
                    r"(?:#\[[^\n]*\]\s*)*(?:async\s+)?fn\s+(\w+)",
                    (root / path).read_text(),
                ))
            assert name in rust_inventory[path], f"missing Rust test: {reference}"
        primary = evidence[0].split("::")[0] if evidence else None
        assert entry.get("related_rust_coverage") == primary, f"stale primary evidence: {key}"
    assert used == set(groups), f"unused audit groups: {set(groups) - used}"
    return counts


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
    counts = verify_audit(root, mapping)
    print(f"Verified {len(manifest['files'])} frozen files and {len(tests)} mapped regressions.")
    print("Reviewed retention: " + ", ".join(f"{counts[key]} {key}" for key in sorted(counts)))


if __name__ == "__main__":
    main()
