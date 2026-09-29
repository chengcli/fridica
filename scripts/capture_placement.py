"""Capture exact machine choice, diagnostics, GPU slicing and payloads from v0.3.11.

Run with PYTHONPATH=spec. Fixtures are synthetic and contain no owner data.
"""
from dataclasses import asdict
from itertools import product
import json
from pathlib import Path
import tomllib

from fridica.config.loader import parse
from fridica.core.errors import MatchError
from fridica.machines import Selector, resolve
from fridica.machines.registry import slot_gpus

SOURCE = '''
[owner]
slack_user = "UOWNER"
[slack]
workspace = "TTEAM"
channels = ["CROOM"]
[parent]
default_machine = "cpu"
[machines.cpu]
host = "cpu"
backends = ["claude", "codex"]
[machines.cpu.workspaces]
project = "/work/cpu"
[machines.gpu]
host = "owner@gpu"
tags = ["cuda", "big"]
backends = ["codex", "claude"]
max_jobs = 2
resources = {cpus=32, gpus=[0,1,2,3], gpu_type="Test GPU"}
[machines.gpu.workspaces]
shared = "/work/shared"
unique = "~/work/unique"
[machines.gpu2]
host = "gpu2"
tags = ["cuda"]
backends = ["codex"]
max_jobs = 4
resources = {gpus=[2,3]}
[machines.gpu2.workspaces]
shared = "/work/shared"
'''


def main():
    registry = parse(tomllib.loads(SOURCE), base=Path('/tmp')).machines
    data = {"source": SOURCE, "payload": registry.payload({"gpu": 2}), "cases": [], "gpus": []}
    for machine, tags, workspace, backend, sticky, loads in product(
            ("", "gpu", "missing"), ((), ("cuda",), ("tpu",)), ("", "shared", "unique"),
            ("", "codex", "claude"), ("", "gpu", "gpu2"), ({}, {"gpu": 2, "gpu2": 1})):
        selector = Selector(machine=machine, tags=tags, workspace=workspace, backend=backend)
        row = {"selector": asdict(selector), "sticky_machine": sticky, "sticky_workspace": "shared", "busy": loads}
        try:
            match = resolve(registry, selector, sticky_machine=sticky, sticky_workspace="shared", busy=loads)
            row["expected"] = {"machine": match.machine.name, "workspace": match.workspace.name, "backend": match.backend}
        except MatchError as error:
            row["error"] = {"message": str(error), "candidates": error.candidates}
        data["cases"].append(row)
    for gpus, slot, slots in product((None, (), (0,), (0, 1), (2, 3, 5, 6, 9)), range(0, 6), (1, 2, 3, 4)):
        data["gpus"].append({"gpus": gpus, "slot": slot, "slots": slots, "expected": slot_gpus(gpus, slot, slots)})
    path = Path(__file__).resolve().parents[1] / "tests/corpus/placement.json"
    path.write_text(json.dumps(data, sort_keys=True, separators=(",", ":")) + "\n")
    print(f"Captured {len(data['cases'])} placement cases and {len(data['gpus'])} GPU views.")


if __name__ == "__main__":
    main()
