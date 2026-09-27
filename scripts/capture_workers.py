"""Frozen result/policy projections and exact fake CLI fixtures, without model calls.

These are scoped fixtures. Process scheduling, interrupts, and v0.4 protocol
hardening use independent Rust tests, not inferred historical responses.
"""
from dataclasses import asdict
import importlib.util
import itertools
import json
from pathlib import Path, PurePosixPath

from fridica.machines.registry import Machine, Policy, Resources, Workspace
from fridica.workers.codex import CodexWorker, answer, describe
from fridica.workers.claude import ClaudeWorker
from fridica.workers.protocol import WorkerSpec
from fridica.workers.result import RESULT_SCHEMA, FORMAT_NOTE, SUMMARIZE_PROMPT, coerce, parse, prose, fallback


def plain(value):
    return asdict(value) if value is not None else None


def main():
    root = Path(__file__).resolve().parents[1]
    module = importlib.util.spec_from_file_location("frozen_fakes", root / "spec/tests/fakes.py")
    fakes = importlib.util.module_from_spec(module)
    module.loader.exec_module(fakes)
    for backend, source in (("codex", fakes.FAKE_APP_SERVER), ("claude", fakes.FAKE_CLAUDE)):
        (root / f"tests/corpus/fake_{backend}.py").write_text(source.replace("{python}", "/usr/bin/env python3"))
    valid = {"status": "done", "summary": "Fixed.", "report": "All green."}
    coercions = [None, [], {}, 1, True, "string", valid]
    for key in ("status", "summary", "changes", "validation", "artifacts", "machine_state", "unresolved", "question", "report"):
        for value in (None, [], {}, 1, True, "", "雪" * 4500):
            coercions.append({**valid, key: value})
    coercions += [
        {**valid, "changes": ["skip", {"path": "x", "change": "invalid", "note": "n" * 1000}] * 40,
         "validation": [{"command": "pytest", "outcome": "invalid", "detail": "d" * 500}] * 40,
         "artifacts": [{"path": "/w/a.exe", "kind": "exe"}, *[{"path": "/w/a.md", "kind": "md", "caption": "c" * 400}] * 5],
         "unresolved": [False, "雪" * 1000] * 40, "machine_state": {"dirty": 1, "branch": "b" * 300}},
        {**valid, "changes": [{}] * 30 + [{"path": "outside-first-30"}], "artifacts": [{}] * 30 + [{"path": "/w/a.md", "kind": "md"}]},
    ]
    body = json.dumps(valid)
    texts = [body, "prose", "", "\n   \t", "\x1c\x1d\x1e\x1f", "prefix\n" + body, "```json\n{broken\n```", "```python\nx=1\n```\n```json\n" + body + "\n```"]
    for label, ending, spacing in itertools.product(("json", "python", "c++", "雪", "", "bad label"), ("", "\n", "\ntrailing"), ("", " \t")):
        texts.append(f"before\n```{label}{spacing}\n{body}\n```{spacing}{ending}")
    texts += ["```\n```\n```json\n"+body+"\n```", "```json\n"+json.dumps({**valid,"summary":"old"})+"\n```\n```json\n"+body+"\n```", "雪"*5000]
    results = [{"input": value, "expected": plain(coerce(value))} for value in coercions]
    parses = [{"text": text, "parse": plain(parse(text)), "prose": prose(text), "fallback": plain(fallback(text))} for text in texts]
    policies = []
    for backend, mode, approvals, confined, network in itertools.product(("codex", "claude"), ("read-only", "write", "full"), ("never", "on-request", "untrusted", "auto"), (False, True), (False, True)):
        policy = Policy(mode=mode, approvals=approvals, gpu_confine=confined, network=("github.com",) if network else ())
        workspace = Workspace("work", PurePosixPath("~/repo"), policy)
        machine = Machine("box", "ssh", (workspace,), ("codex", "claude"), backend, policy, host="box", resources=Resources(cpus=4))
        spec = WorkerSpec("w1", machine, workspace, backend, "Owner instructions.", model="model", reasoning_effort="high", excluded_env=("SLACK_USER_TOKEN",))
        worker = CodexWorker(spec) if backend == "codex" else ClaudeWorker(spec)
        # Fixed identifiers isolate command construction from random session IDs.
        worker.resume = "previous-session"
        worker.session = "previous-session"
        expected = {"command": worker.command()}
        if backend == "codex":
            expected.update(sandbox_mode=worker.sandbox_mode(), sandbox_policy=worker.sandbox_policy())
        else:
            expected["settings"] = worker.settings()
        policies.append({"spec": asdict(spec), "expected": expected})
    approvals = []
    for method, kind, params in [
        ("item/commandExecution/requestApproval", "command", {"command": ["make", "test"], "cwd": "/work"}),
        ("execCommandApproval", "command", {"command": "make", "reason": "check"}),
        ("applyPatchApproval", "file_change", {"grantRoot": "/work", "changes": {}}),
        ("item/permissions/requestApproval", "permissions", {"permissions": {"network": {"enabled": True}}}),
    ]:
        for decision in ("once", "session", "deny"):
            approvals.append({"method": method, "kind": kind, "params": params, "decision": decision,
                              "answer": answer(method, kind, params, decision), "description": asdict(describe(kind, params, "123"))})
    corpus = {"schema": RESULT_SCHEMA, "format_note": FORMAT_NOTE, "summarize_prompt": SUMMARIZE_PROMPT,
              "coercions": results, "parses": parses, "policies": policies, "approvals": approvals,
              "exceptions": {"codex_command_suffix": [],
                             "reason": "No configured Fridica MCP aliases in the frozen fixtures; runtime wiring must supply installed aliases."}}
    (root / "tests/corpus/workers.json").write_text(json.dumps(corpus, ensure_ascii=False, sort_keys=True, separators=(",", ":"), default=str) + "\n")
    print(f"Captured {len(results)} result objects, {len(parses)} text results, {len(policies)} policies, and {len(approvals)} approvals.")


if __name__ == "__main__":
    main()
