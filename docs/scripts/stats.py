"""Every number in the design document, computed from snapshots of Fridica's state databases.

``collect_new`` reads the current schema (``src/fridica/store/schema.py``); ``collect_legacy`` reads the
pre-overhaul schema (events/tasks/collaboration). Both return plain dicts and lists so the charts, tables,
and tests share one source of truth. No plotting imports here: this module also runs in CI.
"""

from __future__ import annotations

from collections import Counter, defaultdict
import json
from pathlib import Path
import re
import sqlite3
import statistics
import subprocess

from common import REPO, Anonymizer


def percentile(values: list[float], fraction: float) -> float | None:
    if not values:
        return None
    ordered = sorted(values)
    index = min(len(ordered) - 1, max(0, round(fraction * (len(ordered) - 1))))
    return ordered[index]


def summary(values: list[float]) -> dict:
    return {"n": len(values), "mean": statistics.fmean(values) if values else None,
            "p50": percentile(values, 0.5), "p95": percentile(values, 0.95), "max": max(values) if values else None}


def hourly(timestamps: list[float]) -> dict[int, int]:
    """Counts per hour bucket (hours since the first timestamp)."""
    if not timestamps:
        return {}
    start = min(timestamps)
    counts = Counter(int((stamp - start) // 3600) for stamp in timestamps)
    return {hour: counts.get(hour, 0) for hour in range(max(counts) + 1)}


def rows(db: sqlite3.Connection, sql: str, parameters=()) -> list[sqlite3.Row]:
    return db.execute(sql, parameters).fetchall()


def table_counts(db: sqlite3.Connection) -> dict[str, int]:
    names = [row[0] for row in rows(db, "SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")]
    return {name: rows(db, f"SELECT COUNT(*) FROM {name}")[0][0] for name in names}


def columns(db: sqlite3.Connection, table: str) -> int:
    return len(rows(db, f"PRAGMA table_info({table})"))


def max_concurrency(intervals: list[tuple[float, float]]) -> tuple[int, list[tuple[float, int]]]:
    """Largest number of overlapping intervals, and the step curve of running counts."""
    events = sorted([(start, 1) for start, _ in intervals] + [(end, -1) for _, end in intervals])
    running, peak, curve = 0, 0, []
    for stamp, delta in events:
        running += delta
        peak = max(peak, running)
        curve.append((stamp, running))
    return peak, curve


# ----- current design -----

def collect_new(db: sqlite3.Connection) -> dict:
    meta = {row["key"]: row["value"] for row in rows(db, "SELECT key, value FROM meta")}
    anonymizer = Anonymizer(meta.get("owner", ""))
    data: dict = {"schema_version": int(meta.get("schema_version", 0)), "tables": table_counts(db)}

    messages = rows(db, "SELECT sender, channel, source, meta_json, received_at, verdict FROM messages")
    stamps = [row["received_at"] for row in messages]
    data["span"] = (min(stamps), max(stamps)) if stamps else (0, 0)
    data["days"] = (data["span"][1] - data["span"][0]) / 86400 if stamps else 0
    data["by_source"] = dict(Counter(row["source"] for row in messages))
    data["hourly_by_source"] = {}
    start = data["span"][0]
    for source in data["by_source"]:
        counts = Counter(int((row["received_at"] - start) // 3600) for row in messages if row["source"] == source)
        data["hourly_by_source"][source] = counts
    data["hours"] = int((data["span"][1] - start) // 3600) + 1 if stamps else 0
    data["peer_messages"] = sum(1 for row in messages if row["meta_json"] and row["source"] != "self")
    senders = Counter()
    for row in messages:
        if row["source"] == "self":
            senders["Fridica (owner's posts)"] += 1
        elif row["meta_json"]:
            senders["peer agents"] += 1
        else:
            senders[anonymizer.person(row["sender"])] += 1
    data["senders"] = dict(senders.most_common())
    data["people"] = len({row["sender"] for row in messages if row["source"] != "self" and not row["meta_json"]})
    data["channels"] = len({anonymizer.channel(row["channel"]) for row in messages})
    verdicts = Counter(row["verdict"].split(":", 1)[0] for row in messages if row["verdict"])
    data["verdicts"] = dict(verdicts.most_common())
    data["ingested"] = sum(1 for row in messages if row["source"] != "self")

    threads = rows(db, "SELECT control, status, turns, summary, decisions_json FROM threads")
    data["threads"] = len(threads)
    data["thread_states"] = dict(Counter(f"{row['control']}/{row['status']}" for row in threads))
    data["turns"] = dict(sorted(Counter(row["turns"] for row in threads).items()))
    data["summary_chars"] = summary([len(row["summary"]) for row in threads if row["summary"]])
    data["decisions_per_thread"] = summary([len(json.loads(row["decisions_json"])) for row in threads])

    data["inbox"] = dict(Counter(row[0] for row in rows(db, "SELECT kind FROM thread_inbox")))

    calls = rows(db, "SELECT call, backend, latency_ms, prompt_chars, error FROM parent_turns")
    data["parent"] = {}
    for call in sorted({row["call"] for row in calls}):
        chosen = [row for row in calls if row["call"] == call]
        data["parent"][call] = {
            "count": len(chosen), "errors": sum(1 for row in chosen if row["error"]),
            "latency_s": summary([row["latency_ms"] / 1000 for row in chosen]),
            "prompt_chars": summary([row["prompt_chars"] for row in chosen]),
            "latencies": [row["latency_ms"] / 1000 for row in chosen],
            "prompts": [row["prompt_chars"] for row in chosen],
        }
    data["parent_calls"] = len(calls)
    data["parent_errors"] = sum(1 for row in calls if row["error"])

    workers = rows(db, "SELECT id, session_id, machine, backend, role, ephemeral, slot FROM workers")
    data["workers"] = len(workers)
    data["workers_by_machine_role"] = defaultdict(Counter)
    for row in workers:
        data["workers_by_machine_role"][row["machine"]][row["role"]] += 1
    data["workers_by_machine_role"] = {machine: dict(roles) for machine, roles in data["workers_by_machine_role"].items()}
    data["ephemeral_workers"] = sum(row["ephemeral"] for row in workers)
    data["backends"] = dict(Counter(row["backend"] for row in workers))
    data["slots"] = dict(Counter(f"{row['machine']}:{row['slot'] or '-'}" for row in workers))
    per_thread = Counter(row["session_id"] for row in workers)
    data["workers_per_thread"] = dict(sorted(Counter(per_thread.values()).items()))
    data["max_workers_in_thread"] = max(per_thread.values()) if per_thread else 0
    machine_of = {row["id"]: row["machine"] for row in workers}
    role_of = {row["id"]: row["role"] for row in workers}

    jobs = rows(db, "SELECT worker_id, join_group, status, queued_at, started_at, finished_at, result_json FROM jobs")
    data["jobs"] = len(jobs)
    data["job_status"] = dict(Counter(row["status"] for row in jobs))
    finished = [row for row in jobs if row["started_at"] and row["finished_at"] and row["finished_at"] >= row["started_at"]]
    data["job_minutes"] = summary([(row["finished_at"] - row["started_at"]) / 60 for row in finished if row["status"] == "done"])
    data["job_minutes_all"] = [(row["finished_at"] - row["started_at"]) / 60 for row in finished if row["status"] == "done"]
    data["queue_wait_s"] = summary([row["started_at"] - row["queued_at"] for row in jobs if row["started_at"]])
    groups = Counter(row["join_group"] for row in jobs if row["join_group"])
    data["group_sizes"] = dict(sorted(Counter(groups.values()).items()))
    data["fan_out_groups"] = sum(1 for size in groups.values() if size > 1)
    data["jobs_by_machine_status"] = defaultdict(Counter)
    data["jobs_by_role"] = Counter()
    for row in jobs:
        data["jobs_by_machine_status"][machine_of.get(row["worker_id"], "?")][row["status"]] += 1
        data["jobs_by_role"][role_of.get(row["worker_id"], "?")] += 1
    data["jobs_by_machine_status"] = {machine: dict(counts) for machine, counts in data["jobs_by_machine_status"].items()}
    data["jobs_by_role"] = dict(data["jobs_by_role"].most_common())
    intervals = [(row["started_at"], row["finished_at"]) for row in finished]
    data["peak_concurrency"], data["concurrency_curve"] = max_concurrency(intervals)
    data["concurrency_curve"] = [(stamp - start, running) for stamp, running in data["concurrency_curve"]]
    per_machine_peak = {}
    for machine in set(machine_of.values()):
        chosen = [(row["started_at"], row["finished_at"]) for row in finished if machine_of.get(row["worker_id"]) == machine]
        per_machine_peak[machine] = max_concurrency(chosen)[0]
    data["peak_by_machine"] = per_machine_peak
    results = [json.loads(row["result_json"]) for row in jobs if row["result_json"]]
    data["result_status"] = dict(Counter(result.get("status", "?") for result in results).most_common())
    data["result_summary_chars"] = summary([len(result.get("summary", "")) for result in results])
    data["result_report_chars"] = summary([len(result.get("report", "")) for result in results])
    data["result_changes"] = summary([len(result.get("changes", [])) for result in results])
    data["result_validation"] = summary([len(result.get("validation", [])) for result in results])

    artifacts = rows(db, "SELECT kind, status, size FROM artifacts")
    data["artifacts"] = dict(Counter(f"{row['kind']}/{row['status']}" for row in artifacts))
    data["artifact_kb"] = summary([row["size"] / 1024 for row in artifacts if row["size"]])

    outbox = rows(db, "SELECT idem_key, kind, state, text FROM outbox")
    data["outbox"] = defaultdict(Counter)
    for row in outbox:
        data["outbox"][row["kind"]][row["state"]] += 1
    data["outbox"] = {kind: dict(states) for kind, states in data["outbox"].items()}
    data["posts"] = len(outbox)
    data["posts_unsent"] = sum(1 for row in outbox if row["state"] != "sent")
    data["reply_chars"] = summary([len(row["text"]) for row in outbox if row["kind"] == "reply"])
    data["report_chars"] = summary([len(row["text"]) for row in outbox if row["kind"] == "report"])
    data["reply_lengths"] = [len(row["text"]) for row in outbox if row["kind"] in ("reply", "report")]
    data["post_kinds"] = dict(Counter(row["kind"] for row in outbox).most_common())
    data["posts_text"] = sum(1 for row in outbox if row["kind"] != "upload")
    # A report posted for an inbox item that has no parent call went through the report fast path: the worker's own
    # Slack-ready report was posted as is.
    called = {row[0] for row in rows(db, "SELECT DISTINCT inbox_id FROM parent_turns")}
    reports = [row["idem_key"] for row in outbox if row["kind"] == "report" and row["idem_key"].endswith(":report")]
    data["fast_path_reports"] = sum(1 for key in reports if key.split(":")[0].isdigit() and int(key.split(":")[0]) not in called)

    notes = rows(db, "SELECT session_id, revision FROM notes")
    data["note_threads"] = len({row["session_id"] for row in notes})
    data["note_revisions"] = len(notes)
    data["approvals"] = rows(db, "SELECT COUNT(*) FROM approvals")[0][0]
    data["audit"] = dict(Counter(row[0] for row in rows(db, "SELECT action FROM audit")))
    return data


# ----- legacy design -----

def collect_legacy(db: sqlite3.Connection) -> dict:
    data: dict = {"tables": table_counts(db), "task_columns": columns(db, "tasks"), "event_columns": columns(db, "events")}
    events = rows(db, "SELECT event_id, timestamp, state, decision, result, payload FROM events")
    # The legacy daemon also stored its own posts as events (ids "outgoing:…") to recognize their echoes.
    data["own_posts"] = sum(1 for row in events if row["event_id"].startswith("outgoing:"))
    data["inbound"] = len(events) - data["own_posts"]
    stamps = [row["timestamp"] for row in events]
    data["span"] = (min(stamps), max(stamps)) if stamps else (0, 0)
    data["days"] = (data["span"][1] - data["span"][0]) / 86400 if stamps else 0
    data["events"] = len(events)
    data["hourly"] = hourly(stamps)
    data["states"] = dict(Counter(row["state"] for row in events))
    data["decisions"] = dict(Counter(row["decision"] or "(none)" for row in events).most_common())
    replies = []
    for row in events:
        if row["result"]:
            try:
                text = json.loads(row["result"]).get("text", "")
            except (ValueError, AttributeError):
                text = ""
            if text:
                replies.append(len(text))
    data["reply_chars"] = summary(replies)
    data["sent"] = data["states"].get("sent", 0)
    senders = set()
    for row in events:
        try:
            senders.add(json.loads(row["payload"]).get("sender_id"))
        except ValueError:
            pass
    data["senders"] = len(senders)
    tasks = rows(db, "SELECT status, control_state, turns, session, continuation, worker_state, worker_host FROM tasks")
    data["tasks"] = len(tasks)
    data["task_states"] = dict(Counter(f"{row['control_state']}/{row['status']}" for row in tasks))
    data["turns"] = dict(sorted(Counter(row["turns"] for row in tasks).items()))
    data["sessions"] = sum(1 for row in tasks if row["session"])
    data["continuations"] = sum(1 for row in tasks if row["continuation"] and row["continuation"][0].isdigit())
    data["heavy_jobs"] = sum(1 for row in tasks if row["worker_state"])
    data["heavy_by_host"] = dict(Counter(row["worker_host"] for row in tasks if row["worker_host"]))
    data["collaboration_revisions"] = data["tables"].get("collaboration_history", 0)
    return data


# ----- repository -----

PACKAGES = ("app.py", "core", "store", "config", "machines", "slack", "threads", "parent", "workers", "exec",
            "approvals", "control", "dashboard", "doctor", "cli")


def source_lines() -> dict[str, int]:
    root = REPO / "src" / "fridica"
    counts = {}
    for package in PACKAGES:
        path = root / package
        files = [path] if path.is_file() else sorted(path.rglob("*.py"))
        counts[package] = sum(len(file.read_text().splitlines()) for file in files)
    return counts


def test_counts() -> dict[str, int]:
    tests = sorted((REPO / "tests").glob("test_*.py"))
    functions = sum(len(re.findall(r"^def test_", file.read_text(), re.M)) for file in tests)
    return {"files": len(tests), "functions": functions}


def pull_requests() -> list[tuple[str, str, int]]:
    """(date, title, number) for every merged PR on the current branch, oldest first."""
    try:
        log = subprocess.run(["git", "log", "--reverse", "--format=%ad%x09%s", "--date=format:%m-%d"],
                             cwd=REPO, capture_output=True, text=True, check=True).stdout
    except (OSError, subprocess.CalledProcessError):
        return []
    found = []
    for line in log.splitlines():
        date, _, subject = line.partition("\t")
        match = re.search(r"\(#(\d+)\)$", subject)
        if match:
            found.append((date, subject[:match.start()].strip(), int(match.group(1))))
    return found


def legacy_ddl_modules() -> int:
    """Modules that ran CREATE TABLE just before the overhaul."""
    try:
        log = subprocess.run(["git", "log", "--format=%H %s"], cwd=REPO, capture_output=True, text=True, check=True).stdout
        commit = next(line.split()[0] for line in log.splitlines() if line.endswith("(#22)"))
        found = subprocess.run(["git", "grep", "-l", "CREATE TABLE", f"{commit}^", "--", "src"], cwd=REPO,
                               capture_output=True, text=True).stdout
        return len(found.split())
    except (OSError, subprocess.CalledProcessError, StopIteration):
        return 0


def legacy_lines() -> int:
    """Lines of Python in src/fridica just before the overhaul (the parent of the PR #22 merge)."""
    try:
        log = subprocess.run(["git", "log", "--format=%H %s"], cwd=REPO, capture_output=True, text=True, check=True).stdout
        commit = next(line.split()[0] for line in log.splitlines() if line.endswith("(#22)"))
        files = subprocess.run(["git", "ls-tree", "-r", "--name-only", f"{commit}^", "src/fridica"], cwd=REPO,
                               capture_output=True, text=True, check=True).stdout.split()
        return sum(len(subprocess.run(["git", "show", f"{commit}^:{name}"], cwd=REPO, capture_output=True, text=True,
                                      check=True).stdout.splitlines()) for name in files if name.endswith(".py"))
    except (OSError, subprocess.CalledProcessError, StopIteration):
        return 0


def defaults() -> dict[str, dict]:
    """The configuration defaults, read from the code rather than restated."""
    from dataclasses import fields
    from fridica.config.schema import Limits, ParentConfig
    from fridica.machines.registry import Machine, Policy
    return {
        "limits": {field.name: field.default for field in fields(Limits)},
        "parent": {field.name: field.default for field in fields(ParentConfig) if field.name != "repos"},
        "machine": {field.name: field.default for field in fields(Machine) if field.name in ("max_workers", "max_jobs")},
        "policy": {field.name: field.default for field in fields(Policy)},
    }


def context_budgets() -> dict[str, int]:
    from fridica.parent import schemas
    from fridica.threads import context
    from fridica.workers import result
    return {"history_messages": context.HISTORY_LIMIT, "channel_messages": context.CHANNEL_LIMIT,
            "channel_chars": context.CHANNEL_CHARS, "summary_chars": schemas.SUMMARY_CHARS,
            "max_decisions": schemas.MAX_DECISIONS, "reply_chars": schemas.REPLY_CHARS,
            "details_chars": schemas.DETAILS_CHARS, "brief_chars": schemas.BRIEF_CHARS,
            "result_summary": result.SUMMARY_LIMIT, "result_report": result.REPORT_LIMIT,
            "max_artifacts": result.MAX_ARTIFACTS}


def machines(config_path: Path | None) -> list[dict]:
    """The configured machines (names, transport, tags, GPUs, slots); empty when no config is readable."""
    try:
        from fridica.config import DEFAULT_CONFIG, load_config
        config = load_config(config_path or DEFAULT_CONFIG)
    except Exception:
        return []
    return [{"name": machine.name, "transport": machine.transport, "tags": list(machine.tags),
             "backends": list(machine.backends), "gpus": list(machine.resources.gpus or ()),
             "gpu_type": machine.resources.gpu_type, "max_jobs": machine.max_jobs, "max_workers": machine.max_workers,
             "workspaces": len(machine.workspaces), "subfolders": any(item.subfolders for item in machine.workspaces)}
            for machine in config.machines.machines]


def anonymize_machines(new: dict, old: dict | None, configured: list[dict]) -> list[str]:
    """Replace machine names by Node 1, 2, … (configured order first) everywhere; returns the real names."""
    from common import Anonymizer
    names = Anonymizer()
    for machine in configured:
        names.machine(machine["name"])
    seen = set(new["jobs_by_machine_status"]) | set(new["workers_by_machine_role"]) | set(new["peak_by_machine"])
    if old:
        seen |= set(old["heavy_by_host"])
    for name in sorted(seen - {"?"}):
        names.machine(name)
    rename = lambda mapping: {names.machine(key) if key != "?" else key: value for key, value in mapping.items()}  # noqa: E731
    for key in ("jobs_by_machine_status", "workers_by_machine_role", "peak_by_machine"):
        new[key] = rename(new[key])
    new["slots"] = {f"{names.machine(key.rpartition(':')[0])}:{key.rpartition(':')[2]}": value
                    for key, value in new["slots"].items()}
    if old:
        old["heavy_by_host"] = rename(old["heavy_by_host"])
    for machine in configured:
        machine["name"] = names.machine(machine["name"])
    return list(names.machines)


def collect(state: Path, legacy: Path | None, config: Path | None = None) -> dict:
    from common import snapshot
    with snapshot(state) as db:
        new = collect_new(db)
    old = None
    if legacy is not None and legacy.exists():
        with snapshot(legacy) as db:
            old = collect_legacy(db)
    configured = machines(config)
    real_names = anonymize_machines(new, old, configured)
    return {"new": new, "legacy": old, "lines": source_lines(), "tests": test_counts(), "prs": pull_requests(),
            "legacy_lines": legacy_lines(), "legacy_ddl_modules": legacy_ddl_modules(),
            "defaults": defaults(), "budgets": context_budgets(), "machines": configured,
            # Only for the build's privacy check; never written anywhere.
            "private_machine_names": real_names}
