"""Build the Fridica design document: statistics → figures and generated RST → PDF.

    python docs/scripts/build.py [--state PATH] [--legacy PATH] [--config PATH] [--no-pdf]

The state databases are read through private snapshots (never modified). Everything the build writes is
aggregate and anonymized; a final check refuses to finish if a Slack ID appears in any generated file.
"""

from __future__ import annotations

import argparse
import datetime as dt
from pathlib import Path
import re
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

from common import (  # noqa: E402
    DEFAULT_LEGACY, DEFAULT_STATE, DOC, FIGURES, GENERATED, RST, assert_anonymous, list_table, number, substitutions,
)
import stats  # noqa: E402

RESPONSIBILITY = {
    "app.py": "Composition root: wires store, Slack, parent, actors, supervisor, control API; recovery; hot reload",
    "core": "Value types (sessions, workers, jobs, results, posts), errors, doorbell bus, injectable clock",
    "store": "SQLite: the only DDL (versioned migrations), one repository per table, crash recovery",
    "config": "Typed schema with the single set of defaults, loader with every cross-field rule, editor, discovery",
    "machines": "Machine registry, policies, resources, GPU/slot views, selector resolution",
    "slack": "Socket Mode ingress, Web API egress, durable outbox dispatcher, catch-up, links, rendering, metadata",
    "threads": "One serial actor per thread, pure gate/advance policy, bounded parent context",
    "parent": "Tool-less structured-output LLM calls, prompts, schemas, action validation and repair, contract, repos",
    "workers": "JSONL worker processes (Claude, Codex), WorkerResult parsing, artifacts, supervisor with slots",
    "exec": "Transports (local, ssh, slurm stub), process plumbing, SSH watchdog, bubblewrap confinement",
    "approvals": "Approval broker (persist, wait, timeout) and policy rules",
    "control": "Daemon JSON API on a 0600 Unix socket and its client",
    "dashboard": "Localhost web UI proxying the control API behind a per-run key",
    "doctor": "Environment, backend, sandbox, and GPU checks on every machine",
    "cli": "Command line: init, configure, doctor, start, dashboard, control commands",
}

SCHEMA_PURPOSE = {
    "messages": "every Slack message, including Fridica's own posts (source=self)",
    "threads": "thread sessions: control, status, turns, summary, decisions, sticky context",
    "thread_inbox": "per-thread work queue: messages, worker results, controls, debriefs",
    "parent_turns": "one row per parent LLM call, written with the effects it produced",
    "workers": "Claude/Codex sessions bound to machine, workspace, slot",
    "jobs": "one delegated task each; results as WorkerResult JSON",
    "artifacts": "files returned by jobs, validated and stored for upload",
    "outbox": "every Slack post, idempotent by key, ordered per thread",
    "approvals": "worker permission requests and decisions",
    "notes": "revisioned task notes per thread",
    "audit": "owner and policy actions",
    "cooldowns": "per-channel pacing of unsolicited replies",
    "runtime": "daemon heartbeat, Slack status, config fingerprint",
    "meta": "schema version and the bound Slack identity",
}


def when(stamp: float) -> str:
    return dt.datetime.fromtimestamp(stamp).strftime("%Y-%m-%d %H:%M") if stamp else "n/a"


def setting(key: str, value) -> str:
    """A configuration default as a reader would write it."""
    if key == "default_machine":
        return "the first configured machine"
    if key == "gpu_confine" and value is None:
        return "automatic (write-mode workspaces on GPU machines)"
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, float) and (key.endswith("timeout") or key in ("worker_idle",)):
        hours = value / 3600
        if hours >= 48:
            return f"{hours / 24:g} days"
        return f"{hours:g} h" if hours >= 1 else f"{value / 60:g} min"
    if isinstance(value, tuple):
        return ", ".join(value) or "(none)"
    return str(value) if value != "" else "(backend CLI default)"


def write(name: str, text: str) -> None:
    GENERATED.mkdir(parents=True, exist_ok=True)
    (GENERATED / name).write_text(text)


def numbers(s: dict, snapshot_time: str) -> dict:
    n, old = s["new"], s["legacy"] or {}
    parent = n["parent"]
    values = {
        "snapshot": snapshot_time,
        "new_start": when(n["span"][0]), "new_end": when(n["span"][1]), "new_hours": number(n["days"] * 24, 1),
        "messages": number(n["tables"]["messages"]), "ingested": number(n["ingested"]),
        "socket_msgs": number(n["by_source"].get("socket", 0)), "catchup_msgs": number(n["by_source"].get("catchup", 0)),
        "self_msgs": number(n["by_source"].get("self", 0)), "peer_msgs": number(n["peer_messages"]),
        "people": n["people"], "channels": n["channels"],
        "threads": number(n["threads"]), "inbox_items": number(sum(n["inbox"].values())),
        "observe": number(n["verdicts"].get("observe", 0)), "respond": number(n["verdicts"].get("respond", 0)),
        "ignore": number(n["verdicts"].get("ignore", 0)), "notice": number(n["verdicts"].get("notice", 0)),
        "parent_calls": number(n["parent_calls"]), "parent_errors": number(n["parent_errors"]),
        "triage_calls": number(parent.get("triage", {}).get("count", 0)),
        "decide_calls": number(parent.get("decide", {}).get("count", 0)),
        "repair_calls": number(parent.get("repair", {}).get("count", 0)),
        "triage_p50": number(parent.get("triage", {}).get("latency_s", {}).get("p50"), 1),
        "decide_p50": number(parent.get("decide", {}).get("latency_s", {}).get("p50"), 1),
        "decide_p95": number(parent.get("decide", {}).get("latency_s", {}).get("p95"), 1),
        "decide_prompt_mean": number(parent.get("decide", {}).get("prompt_chars", {}).get("mean")),
        "decide_prompt_max": number(parent.get("decide", {}).get("prompt_chars", {}).get("max")),
        "triage_prompt_mean": number(parent.get("triage", {}).get("prompt_chars", {}).get("mean")),
        "workers": number(n["workers"]), "ephemeral": number(n["ephemeral_workers"]),
        "max_workers_thread": n["max_workers_in_thread"], "jobs": number(n["jobs"]),
        "jobs_done": number(n["job_status"].get("done", 0)), "jobs_interrupted": number(n["job_status"].get("interrupted", 0)),
        "jobs_failed": number(n["job_status"].get("failed", 0)),
        "job_p50_min": number(n["job_minutes"]["p50"], 1), "job_p95_min": number(n["job_minutes"]["p95"], 1),
        "job_max_min": number(n["job_minutes"]["max"], 1),
        "queue_p50_s": number(n["queue_wait_s"]["p50"], 1), "queue_mean_s": number(n["queue_wait_s"]["mean"], 1),
        "queue_p95_s": number(n["queue_wait_s"]["p95"], 1),
        "fan_out": number(n["fan_out_groups"]), "peak": n["peak_concurrency"],
        "results_done": number(n["result_status"].get("done", 0)), "results_partial": number(n["result_status"].get("partial", 0)),
        "results_failed": number(n["result_status"].get("failed", 0)),
        "results_input": number(n["result_status"].get("needs_input", 0)),
        "summary_mean": number(n["result_summary_chars"]["mean"]), "report_mean": number(n["result_report_chars"]["mean"]),
        "reply_mean": number(n["reply_chars"]["mean"]), "reply_p95": number(n["reply_chars"]["p95"]),
        "thread_summary_mean": number(n["summary_chars"]["mean"]),
        "validation_mean": number(n["result_validation"]["mean"], 1),
        "artifacts": number(sum(int(value) for value in n["artifacts"].values())),
        "artifact_kb": number(n["artifact_kb"]["mean"], 1),
        "posts": number(n["posts"]), "posts_text": number(n["posts_text"]), "posts_unsent": number(n["posts_unsent"]),
        "replies": number(n["post_kinds"].get("reply", 0)), "reports": number(n["post_kinds"].get("report", 0)),
        "uploads": number(n["post_kinds"].get("upload", 0)),
        "fast_path": number(n["fast_path_reports"]),
        "fast_path_share": f"{100 * n['fast_path_reports'] / max(1, n['post_kinds'].get('report', 0)):.0f}%",
        "note_threads": number(n["note_threads"]), "note_revisions": number(n["note_revisions"]),
        "approvals": number(n["approvals"]), "schema_version": n["schema_version"], "tables": len(n["tables"]),
        "lines": number(sum(s["lines"].values())), "test_functions": s["tests"]["functions"], "test_files": s["tests"]["files"],
        "prs": len(s["prs"]), "machines": len(s["machines"]),
        "gpu_machines": sum(1 for machine in s["machines"] if machine["gpus"]),
        "joined": number(sum(count for turns, count in n["turns"].items() if turns > 0)),
        "never_joined": number(n["turns"].get(0, 0)),
        "max_turns": max(n["turns"], default=0),
        "paused_threads": sum(count for state, count in n["thread_states"].items() if state.startswith("paused")),
        "blocked_threads": n["thread_states"].get("active/blocked", 0),
        "decisions_mean": number(n["decisions_per_thread"]["mean"], 1),
        "changes_mean": number(n["result_changes"]["mean"], 2),
        "worker_results": number(n["inbox"].get("worker_result", 0)),
    }
    if old:
        values.update({
            "old_start": when(old["span"][0]), "old_end": when(old["span"][1]), "old_days": number(old["days"], 1),
            "old_events": number(old["events"]), "old_inbound": number(old["inbound"]), "old_posts": number(old["own_posts"]),
            "old_tasks": number(old["tasks"]), "old_task_columns": old["task_columns"],
            "old_heavy": number(old["heavy_jobs"]), "old_sessions": number(old["sessions"]),
            "old_continuations": number(old["continuations"]),
            "old_paused": number(old["decisions"].get("paused", 0)), "old_silent": number(old["decisions"].get("silent", 0)),
            "old_reply_mean": number(old["reply_chars"]["mean"]),
            "old_max_turns": max(old["turns"], default=0),
            "old_at_cap": old["turns"].get(max(old["turns"], default=0), 0),
            "old_threads": number(sum(old["turns"].values())),
            "old_revisions": number(old["collaboration_revisions"]),
            "old_heavy_main": max(old["heavy_by_host"].values(), default=0),
            "old_hosts": len(old["heavy_by_host"]),
            "old_lines": number(s["legacy_lines"]),
            "jobs_per_day_new": number(n["jobs"] / max(n["days"], 1e-9)),
            "jobs_per_day_old": number(old["heavy_jobs"] / max(old["days"], 1e-9), 1),
        })
    probe = s.get("sandbox") or {}
    for name in ("host", "fridica", "claude"):
        for key in ("visible_pids", "dev_entries"):
            values[f"sb_{name}_{key.split('_')[0]}"] = (probe.get(name) or {}).get(key, "n/a")
    values.update({"sb_kernel": probe.get("kernel", "n/a"), "sb_bwrap": probe.get("bwrap", "n/a"),
                   "sb_claude_version": probe.get("claude_version", "n/a"),
                   "sb_claude_measured": probe.get("claude_measured", "n/a"), "sb_measured": probe.get("measured", "n/a"),
                   "sb_userns": probe.get("userns_restricted", "n/a")})
    return {key: str(value) for key, value in values.items()}


def tables(s: dict) -> None:
    n, old = s["new"], s["legacy"]
    rows = [["Overhaul (current)", f"{when(n['span'][0])} → {when(n['span'][1])}",
             f"{number(n['tables']['messages'])} messages, {number(n['threads'])} threads, {number(n['workers'])} workers, "
             f"{number(n['jobs'])} jobs, {number(n['parent_calls'])} parent calls, {number(n['posts'])} posts"]]
    if old:
        rows.append(["Legacy (backup)", f"{when(old['span'][0])} → {when(old['span'][1])}",
                     f"{number(old['events'])} events ({number(old['inbound'])} inbound, {number(old['own_posts'])} own posts), "
                     f"{number(old['tasks'])} tasks, {number(old['heavy_jobs'])} heavy jobs"])
    write("t1_datasets.rst", list_table("Data sets analyzed", ["Design", "Period", "Contents"], rows, [18, 30, 52]))

    lines = s["lines"]
    write("t3_packages.rst", list_table("Packages of src/fridica", ["Package", "Lines", "Responsibility"],
                                        [[package, number(lines[package]), RESPONSIBILITY[package]] for package in lines],
                                        [14, 8, 78]))

    order = ["messages", "threads", "thread_inbox", "parent_turns", "workers", "jobs", "artifacts", "outbox", "approvals",
             "notes", "audit", "cooldowns", "runtime", "meta"]
    write("t4_schema.rst", list_table(f"Schema v{n['schema_version']}: tables and live row counts", ["Table", "Rows", "Holds"],
                                      [[table, number(n["tables"].get(table, 0)), SCHEMA_PURPOSE[table]] for table in order],
                                      [16, 9, 75]))

    parent_rows = []
    for call, values in n["parent"].items():
        latency, prompt = values["latency_s"], values["prompt_chars"]
        parent_rows.append([call, number(values["count"]), number(latency["p50"], 1), number(latency["p95"], 1),
                            number(latency["max"], 1), number(prompt["mean"]), number(prompt["max"]), number(values["errors"])])
    write("t5_parent.rst", list_table("Parent LLM calls", ["Call", "Count", "p50 s", "p95 s", "max s", "Prompt mean",
                                                           "Prompt max", "Errors"], parent_rows))

    statuses = ["done", "interrupted", "failed", "cancelled", "running"]
    job_rows = [[machine, *[number(counts.get(status, 0)) for status in statuses], number(sum(counts.values())),
                 number(n["peak_by_machine"].get(machine, 0))] for machine, counts in sorted(n["jobs_by_machine_status"].items())]
    write("t6_jobs.rst", list_table("Jobs by machine", ["Machine", *statuses, "Total", "Peak concurrent"], job_rows))
    role_rows = [[machine, *[number(roles.get(role, 0)) for role in ("implementer", "tester", "reviewer", "general")],
                  number(sum(roles.values()))] for machine, roles in sorted(n["workers_by_machine_role"].items())]
    write("t6b_workers.rst", list_table("Workers by machine and role", ["Machine", "implementer", "tester", "reviewer",
                                                                         "general", "Total"], role_rows))
    result_rows = [[status, number(count), f"{100 * count / max(1, sum(n['result_status'].values())):.0f}%"]
                   for status, count in n["result_status"].items()]
    write("t7_results.rst", list_table("WorkerResult status", ["Status", "Jobs", "Share"], result_rows, [40, 30, 30]))

    defaults = s["defaults"]
    default_rows = [[f"limits.{key}", setting(key, value)] for key, value in defaults["limits"].items()]
    default_rows += [[f"machines.<name>.{key}", value] for key, value in defaults["machine"].items()]
    default_rows += [[f"policy.{key}", setting(key, value)] for key, value in defaults["policy"].items()]
    default_rows += [[f"parent.{key}", setting(key, value)] for key, value in defaults["parent"].items()]
    write("t10_defaults.rst", list_table("Configuration defaults (read from the code)", ["Setting", "Default"],
                                         default_rows, [55, 45]))

    budgets = s["budgets"]
    budget_rows = [["Thread history sent to the parent", f"last {budgets['history_messages']} messages, "
                    f"≤ context_chars/2 = {defaults['parent']['context_chars'] // 2:,} characters"],
                   ["Channel context (new threads only)", f"{budgets['channel_messages']} messages, ≤ {budgets['channel_chars']:,} "
                    "characters"],
                   ["Triage call history", "last 15 messages"],
                   ["Rolling thread summary", f"≤ {budgets['summary_chars']:,} characters"],
                   ["Decisions kept per thread", f"last {budgets['max_decisions']}"],
                   ["Delegation brief", f"≤ {budgets['brief_chars']:,} characters"],
                   ["WorkerResult.summary / .report", f"≤ {budgets['result_summary']:,} / ≤ {budgets['result_report']:,} "
                    "characters"],
                   ["Artifacts per result", f"≤ {budgets['max_artifacts']}"],
                   ["Slack reply (rest goes to a details file)", f"≤ {budgets['reply_chars']:,} characters"]]
    write("t11_budgets.rst", list_table("Context budgets and caps", ["Item", "Budget"], budget_rows, [45, 55]))

    machine_rows = [[machine["name"], machine["transport"], ", ".join(machine["tags"]) or "none",
                     ", ".join(machine["backends"]),
                     f"{len(machine['gpus'])} × {machine['gpu_type']}" if machine["gpus"] else "none",
                     f"{machine['max_jobs']} / {machine['max_workers']}", "yes" if machine["subfolders"] else "no"]
                    for machine in s["machines"]]
    write("t12_machines.rst", list_table("Configured machines", ["Machine", "Transport", "Tags", "Backends", "GPUs",
                                                                 "Jobs / workers", "Subfolders"], machine_rows))

    probe = s.get("sandbox") or {}
    write("t14_sandbox_env.rst", list_table("Where the sandbox measurements were taken", ["Item", "Value"], [
        ["Machine", "Node 1 (the daemon's own machine), Linux kernel " + probe.get("kernel", "n/a")],
        ["bubblewrap", probe.get("bwrap", "n/a")],
        ["kernel.apparmor_restrict_unprivileged_userns", probe.get("userns_restricted", "n/a")
         + " (1: only profiles such as bwrap's may create user namespaces)"],
        ["Host and Fridica probes", "measured at every build (last " + probe.get("measured", "n/a") + ")"],
        ["Claude probe", f"{probe.get('claude_version', 'n/a')}, measured {probe.get('claude_measured', 'n/a')} "
         "(rerun with --probe-claude)"],
    ], [40, 60]))
    write("t13_prs.rst", list_table("Pull requests", ["Date", "PR", "Title"],
                                    [[date, f"#{number_}", title] for date, title, number_ in s["prs"]], [12, 8, 80]))

    if old:
        compare = [
            ["Thread processing", "one global asyncio lock for every thread", "one serial actor per thread; threads parallel"],
            ["Work per thread", "one backend session, ≤ 1 heavy worker",
             f"many workers with roles (max {n['max_workers_in_thread']} in one thread), fan-out and joins"],
            ["Machines", "derived from host:/path roots; primary host runs every turn",
             "machine registry with tags, policies, slots, GPU split"],
            ["Reply agent", "CLI run with tools, per turn", "tool-less parent; tools only in workers"],
            ["Durability of posts", "replies only; notices, wrap-ups, debriefs best effort",
             f"every post through the outbox ({number(n['posts'])} posts, {n['posts_unsent']} unsent)"],
            ["Schema", f"DDL in {s['legacy_ddl_modules']} modules; tasks row with {old['task_columns']} columns",
             f"one versioned schema (v{n['schema_version']}), {len(n['tables'])} tables"],
            ["Worker output", "free prose, ATTACH: lines", "structured WorkerResult (status, changes, validation, artifacts)"],
            ["Loop control", f"turn limit with wrap-ups ({old['continuations']} continuations)",
             "wait-streak and no-progress pauses; no turn limit"],
            ["Permissions", "Fridica-executed file operations with approvals", "backend sandboxes + native approvals (auto)"],
            ["Worker jobs per day", f"{old['heavy_jobs'] / max(old['days'], 1e-9):.1f}", f"{n['jobs'] / max(n['days'], 1e-9):.0f}"],
            ["Mean Slack reply", f"{number(old['reply_chars']['mean'])} characters", f"{number(n['reply_chars']['mean'])} characters"],
        ]
        write("t9_comparison.rst", list_table("Legacy vs overhaul", ["Aspect", "Legacy (PR #1–#21)", "Overhaul (PR #22–)"],
                                              compare, [20, 38, 42]))


def figures(s: dict) -> list[str]:
    import charts
    import diagrams
    return diagrams.draw_all(s) + charts.draw_all(s)


def leaked_machines(text: str, names: list[str]) -> list[str]:
    """Real machine names found in ``text``. "local" is skipped: it is also a transport and an ordinary word."""
    return [name for name in names
            if name != "local" and re.search(rf"(?<![\w-]){re.escape(name)}(?![\w-])", text, re.IGNORECASE)]


def privacy_check(machine_names: list[str]) -> None:
    for path in [*sorted(GENERATED.glob("*.rst")), *sorted(RST.glob("*.rst"))]:
        text = path.read_text()
        assert_anonymous(text, str(path))
        if leaked := leaked_machines(text, machine_names):
            raise SystemExit(f"privacy check failed: machine names {leaked} in {path}")


def pdf_privacy_check(output: Path, machine_names: list[str]) -> None:
    """The same checks on the PDF's text, which also covers text drawn in the figures only when it is vector text."""
    try:
        text = subprocess.run(["pdftotext", str(output), "-"], capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        print("pdftotext not available; PDF text not checked", file=sys.stderr)
        return
    assert_anonymous(text, str(output))
    if leaked := leaked_machines(text, machine_names):
        raise SystemExit(f"privacy check failed: machine names {leaked} in {output}")


def pdf(output: Path) -> None:
    command = ["rst2pdf", "index.rst", "-s", "fridica.yaml", "--fit-background-mode=scale", "-o", str(output)]
    result = subprocess.run(command, cwd=RST, capture_output=True, text=True)
    problems = [line for line in (result.stdout + result.stderr).splitlines() if "ERROR" in line or "WARNING" in line]
    for line in problems:
        print(line, file=sys.stderr)
    if result.returncode:
        raise SystemExit(f"rst2pdf failed with status {result.returncode}")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--state", type=Path, default=DEFAULT_STATE)
    parser.add_argument("--legacy", type=Path, default=DEFAULT_LEGACY)
    parser.add_argument("--config", type=Path, default=None)
    parser.add_argument("--no-pdf", action="store_true")
    parser.add_argument("--probe-claude", action="store_true",
                        help="re-measure Claude's sandbox (one small model call); otherwise reuse the last measurement")
    args = parser.parse_args()
    snapshot_time = dt.datetime.now().strftime("%Y-%m-%d %H:%M")
    data = stats.collect(args.state, args.legacy, args.config)
    import sandbox
    data["sandbox"] = sandbox.collect(probe_claude=args.probe_claude)
    FIGURES.mkdir(parents=True, exist_ok=True)
    written = figures(data)
    tables(data)
    write("numbers.rst", substitutions(numbers(data, snapshot_time)))
    privacy_check(data["private_machine_names"])
    print(f"{len(written)} figures, {len(list(GENERATED.glob('*.rst')))} generated RST files")
    if not args.no_pdf:
        output = DOC / "fridica-design.pdf"
        pdf(output)
        pdf_privacy_check(output, data["private_machine_names"])
        print(f"wrote {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
