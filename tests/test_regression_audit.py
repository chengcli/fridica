"""The reviewed retention ledger must not silently lose evidence or rationale."""
import importlib.util
import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("verify_baseline", ROOT / "scripts/verify_baseline.py")
verifier = importlib.util.module_from_spec(spec)
spec.loader.exec_module(verifier)


def mapping():
    return json.loads((ROOT / "tests/regression-map.json").read_text())


def test_every_baseline_regression_has_reviewed_retention_and_resolvable_evidence():
    data = mapping()
    counts = verifier.verify_audit(ROOT, data)
    assert sum(counts.values()) == data["baseline_tests"] == 389
    assert all(row["status"] == "retained" for row in data["tests"])
    assert all(counts[k] > 0 for k in ("scoped", "changed", "python-only"))


@pytest.mark.parametrize("fault", [
    "group", "reason", "test", "helper", "evidence", "primary", "classification", "unused", "gate",
])
def test_stale_or_misleading_audit_entries_are_rejected(fault):
    data = mapping()
    row = data["tests"][0]
    group = data["audit"]["groups"][row["audit_group"]]
    if fault == "group":
        row.pop("audit_group")
    elif fault == "reason":
        row["reason"] = "generic retained"
    elif fault == "test":
        group["rust_tests"][0] = "tests/approvals.rs::no_such_test"
    elif fault == "helper":
        group["rust_tests"][0] = "tests/approvals.rs::request"
    elif fault == "evidence":
        group["rust_tests"] = []
    elif fault == "primary":
        row["related_rust_coverage"] = "tests/replay.rs"
    elif fault == "classification":
        group["coverage"] = "complete parity"
    elif fault == "unused":
        data["audit"]["groups"]["unreviewed"] = dict(group)
    elif fault == "gate":
        group["next_gate"] = "someday"
    with pytest.raises(AssertionError):
        verifier.verify_audit(ROOT, data)
