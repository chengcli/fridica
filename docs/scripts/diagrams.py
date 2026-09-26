"""Architecture diagrams drawn with matplotlib patches and annotated with live numbers."""

from __future__ import annotations

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.patches import FancyArrowPatch, FancyBboxPatch  # noqa: E402

from common import FIGURES, number  # noqa: E402

NAVY, BLUE, TEAL, ORANGE, RED, GRAY, LIGHT = "#1f3b73", "#3a6fb0", "#2a9d8f", "#e76f51", "#c0392b", "#6c757d", "#eef2f7"
FILL = {"slack": "#fde8d7", "daemon": "#e3ecf8", "parent": "#dbeee9", "worker": "#fff3c4", "store": "#ececec",
        "machine": "#f3e5f5", "danger": "#fbe0dc"}


def canvas(width: float, height: float):
    figure, axes = plt.subplots(figsize=(width, height))
    axes.set_xlim(0, width)
    axes.set_ylim(0, height)
    axes.axis("off")
    return figure, axes


def box(axes, x, y, w, h, title, body="", fill=LIGHT, edge=NAVY, size=10, style="round,pad=0.02,rounding_size=0.08",
        dashed=False):
    axes.add_patch(FancyBboxPatch((x, y), w, h, boxstyle=style, facecolor=fill, edgecolor=edge, linewidth=1.3,
                                  linestyle="--" if dashed else "-"))
    if body:
        axes.text(x + w / 2, y + h * 0.66, title, ha="center", va="center", fontsize=size, fontweight="bold", color=NAVY)
        axes.text(x + w / 2, y + h * 0.3, body, ha="center", va="center", fontsize=size - 2, color="#222")
    else:
        axes.text(x + w / 2, y + h / 2, title, ha="center", va="center", fontsize=size, fontweight="bold", color=NAVY)


def arrow(axes, start, end, label="", color=GRAY, both=False, size=8, offset=(0.0, 0.12), curve=0.0):
    axes.add_patch(FancyArrowPatch(start, end, arrowstyle="<|-|>" if both else "-|>", mutation_scale=12,
                                   color=color, linewidth=1.2, connectionstyle=f"arc3,rad={curve}"))
    if label:
        axes.text((start[0] + end[0]) / 2 + offset[0], (start[1] + end[1]) / 2 + offset[1], label, ha="center",
                  va="center", fontsize=size, color=color, bbox={"facecolor": "white", "edgecolor": "none", "pad": 1})


def save(figure, name: str) -> str:
    FIGURES.mkdir(parents=True, exist_ok=True)
    path = FIGURES / name
    figure.savefig(path, dpi=150, bbox_inches="tight")
    plt.close(figure)
    return name


# ----- F1: architecture overview -----

def architecture(s: dict) -> str:
    n = s["new"]
    figure, axes = canvas(11, 7.2)
    box(axes, 0.3, 6.1, 10.4, 0.8, "Slack workspace",
        f"{n['channels']} channel · {n['people']} people · {number(n['ingested'])} messages in "
        f"({number(n['by_source'].get('catchup', 0))} by catch-up) · {number(n['posts'])} posts out", fill=FILL["slack"])
    box(axes, 0.3, 4.55, 3.2, 1.05, "Slack I/O", "Socket Mode ingress · catch-up\ndurable outbox (every post)")
    box(axes, 3.9, 4.55, 3.2, 1.05, "Thread actors", f"{number(n['threads'])} sessions, one serial actor each\n"
        f"{number(sum(n['inbox'].values()))} inbox items")
    box(axes, 7.5, 4.55, 3.2, 1.05, "Parent agent", f"tool-less, structured output\n{number(n['parent_calls'])} calls "
        f"(triage · decide · repair · debrief)", fill=FILL["parent"])
    box(axes, 3.9, 2.9, 3.2, 1.05, "Worker supervisor", f"slots · limits · approvals\n{number(n['jobs'])} jobs, "
        f"peak {n['peak_concurrency']} concurrent")
    box(axes, 0.3, 2.9, 3.2, 1.05, "SQLite store", f"schema v{n['schema_version']} · {len(n['tables'])} tables\n"
        "inbox · outbox · jobs · audit", fill=FILL["store"])
    box(axes, 7.5, 2.9, 3.2, 1.05, "Control API", "Unix socket (0600)\nCLI · dashboard")
    machines = sorted(n["jobs_by_machine_status"])
    width = 10.4 / max(1, len(machines))
    for index, machine in enumerate(machines):
        jobs = sum(n["jobs_by_machine_status"][machine].values())
        workers = sum(n["workers_by_machine_role"].get(machine, {}).values())
        box(axes, 0.3 + index * width + 0.1, 0.5, width - 0.2, 1.25, f"machine: {machine}",
            f"{workers} workers · {jobs} jobs\nClaude/Codex session per worker", fill=FILL["machine"])
        arrow(axes, (5.5, 2.9), (0.3 + index * width + width / 2, 1.75))
    axes.text(5.5, 2.2, "local process or ssh -T (JSONL over stdio)", ha="center", fontsize=8, color=GRAY)
    arrow(axes, (1.9, 6.1), (1.9, 5.6), both=True)
    arrow(axes, (3.5, 5.07), (3.9, 5.07), "inbox")
    arrow(axes, (7.1, 5.07), (7.5, 5.07), "context →\n← action", offset=(0, 0.42), size=7.5)
    arrow(axes, (5.5, 4.55), (5.5, 3.95), "jobs", offset=(0.3, 0))
    arrow(axes, (3.9, 3.42), (3.5, 3.42), both=True)
    arrow(axes, (7.1, 3.42), (7.5, 3.42), both=True)
    arrow(axes, (5.0, 3.95), (5.0, 4.55), "results", color=TEAL, offset=(-0.4, 0))
    return save(figure, "f1_architecture.png")


# ----- F2: Slack concept mapping -----

def slack_mapping(s: dict) -> str:
    n = s["new"]
    figure, axes = canvas(11, 6.2)
    pairs = [
        ("Workspace + owner's user token", "Parent agent (one per daemon)", "one identity, one Slack app"),
        ("Channel", "Routing namespace", f"{n['channels']} configured channel(s); never tied to a machine"),
        ("Thread (root ts)", "Thread session", f"{number(n['threads'])} sessions: summary, decisions, sticky context"),
        ("Message", "Inbox item (one transaction)", f"{number(n['ingested'])} ingested → {number(n['inbox'].get('message', 0))} "
         "message items"),
        ("Post by Fridica", "Outbox item + FridicaMeta", f"{number(n['posts'])} posts, {n['posts_unsent']} unsent"),
        ("Another owner's Fridica", "Peer agent (metadata v1/v2)", f"{number(n['peer_messages'])} peer messages"),
    ]
    axes.text(2.1, 5.85, "Slack", ha="center", fontsize=13, fontweight="bold", color=ORANGE)
    axes.text(6.6, 5.85, "Fridica", ha="center", fontsize=13, fontweight="bold", color=NAVY)
    for index, (left, right, note) in enumerate(pairs):
        y = 4.95 - index * 0.88
        box(axes, 0.3, y, 3.6, 0.68, left, fill=FILL["slack"], edge=ORANGE, size=9.5)
        box(axes, 4.8, y, 3.6, 0.68, right, fill=FILL["daemon"], size=9.5)
        arrow(axes, (3.9, y + 0.34), (4.8, y + 0.34))
        axes.text(8.6, y + 0.34, note, va="center", fontsize=8, color="#333")
    return save(figure, "f2_slack_mapping.png")


# ----- F3: layer stack -----

LAYERS = [
    ("Interfaces", ["slack", "control", "dashboard", "cli"], FILL["slack"]),
    ("Coordination", ["threads", "parent", "app.py"], FILL["parent"]),
    ("Execution", ["workers", "exec", "machines", "approvals"], FILL["worker"]),
    ("Foundation", ["store", "core", "config", "doctor"], FILL["store"]),
]


def layers(s: dict) -> str:
    lines = s["lines"]
    figure, axes = canvas(11, 5.6)
    widest = max(sum(lines[p] for p in packages) for _, packages, _ in LAYERS)
    for index, (name, packages, fill) in enumerate(LAYERS):
        y = 4.3 - index * 1.25
        axes.text(0.2, y + 0.45, name, fontsize=11, fontweight="bold", color=NAVY, va="center")
        axes.text(0.2, y + 0.12, f"{number(sum(lines[p] for p in packages))} lines", fontsize=8, color=GRAY, va="center")
        x = 2.0
        for package in packages:
            w = max(1.15, 8.3 * lines[package] / widest)
            box(axes, x, y, w - 0.08, 0.9, package.replace(".py", ""), f"{number(lines[package])}", fill=fill,
                size=9 if w > 1.3 else 8)
            x += w
    axes.text(5.5, 0.05, f"src/fridica: {number(sum(lines.values()))} lines · {s['tests']['functions']} test functions in "
              f"{s['tests']['files']} files", ha="center", fontsize=9, color=GRAY)
    return save(figure, "f3_layers.png")


# ----- F4: message funnel -----

def funnel(s: dict) -> str:
    n = s["new"]
    verdicts = n["verdicts"]
    calls = {name: value["count"] for name, value in n["parent"].items()}
    stages = [
        ("Ingested", [("socket", n["by_source"].get("socket", 0), BLUE), ("catch-up", n["by_source"].get("catchup", 0), TEAL)]),
        ("Gate verdict", [(name, verdicts.get(name, 0), color) for name, color in
                          (("observe", "#9db4d3"), ("respond", TEAL), ("ignore", "#c9c9c9"), ("notice", ORANGE))]),
        ("Parent calls", [(name, calls.get(name, 0), color) for name, color in
                          (("triage", "#9db4d3"), ("decide", NAVY), ("repair", RED), ("debrief", ORANGE))]),
        ("Outbox posts", [(name, value, color) for (name, value), color in
                          zip(n["post_kinds"].items(), [TEAL, NAVY, ORANGE, GRAY, RED, BLUE])]),
    ]
    figure, axes = plt.subplots(figsize=(11, 4.6))
    top = max(sum(value for _, value, _ in parts) for _, parts in stages)
    for column, (name, parts) in enumerate(stages):
        bottom, side = 0, 0
        for label, value, color in parts:
            if not value:
                continue
            axes.bar(column, value, bottom=bottom, color=color, width=0.55, edgecolor="white")
            if value / top > 0.035:
                axes.text(column, bottom + value / 2, f"{label} {value}", ha="center", va="center", fontsize=8,
                          color="white" if color in (NAVY, BLUE, TEAL, RED) else "#222")
            else:
                # Small segments are labelled beside the bar, stacked upward so neighbours never overlap.
                label_y = max(bottom + value / 2, side + top * 0.045) if side else bottom + value / 2
                axes.annotate(f"{label} {value}", (column + 0.28, bottom + value / 2), (column + 0.36, label_y),
                              fontsize=7.5, va="center", color="#333", arrowprops={"arrowstyle": "-", "color": GRAY})
                side = label_y
            bottom += value
        axes.text(column, bottom + top * 0.02, f"{bottom}", ha="center", fontsize=9, fontweight="bold")
    axes.set_xticks(range(len(stages)), [name for name, _ in stages], fontsize=10)
    axes.set_ylabel("count")
    axes.spines[["top", "right"]].set_visible(False)
    axes.set_title("From Slack message to Slack post: every stage is a durable row", fontsize=11, color=NAVY)
    return save(figure, "f4_funnel.png")


# ----- F5: context flow -----

def panel(axes, x, y, w, h, title, lines, fill=LIGHT, size=8.2):
    """A box with a title at the top and left-aligned lines below it."""
    axes.add_patch(FancyBboxPatch((x, y), w, h, boxstyle="round,pad=0.02,rounding_size=0.08", facecolor=fill,
                                  edgecolor=NAVY, linewidth=1.3))
    axes.text(x + w / 2, y + h - 0.22, title, ha="center", va="center", fontsize=10, fontweight="bold", color=NAVY)
    step = (h - 0.5) / max(1, len(lines))
    for index, line in enumerate(lines):
        axes.text(x + 0.14, y + h - 0.55 - step * index, line, ha="left", va="center", fontsize=size, color="#222")


def context_flow(s: dict) -> str:
    n, b, parent = s["new"], s["budgets"], s["new"]["parent"].get("decide", {}).get("prompt_chars", {})
    history = s["defaults"]["parent"]["context_chars"] // 2
    figure, axes = canvas(11, 7.1)
    panel(axes, 0.2, 4.2, 3.2, 2.2, "Worker session", [
        "files, commands, test logs, diffs", "tool output of any size", "managed by Claude or Codex,",
        "resumed across jobs of one worker", "stays on its machine"], fill=FILL["worker"])
    panel(axes, 0.2, 1.3, 3.2, 2.3, "WorkerResult", [
        f"status · summary ≤ {b['result_summary']:,}", f"report ≤ {b['result_report']:,} (Slack-ready)",
        "changes · validation · unresolved", f"artifacts ≤ {b['max_artifacts']} (png, pdf, md)",
        f"measured: summary {number(n['result_summary_chars']['mean'])},", f"report {number(n['result_report_chars']['mean'])} "
        "characters (mean)"], fill=FILL["worker"])
    panel(axes, 3.9, 1.3, 3.6, 5.1, "Parent prompt (one call)", [
        "contract + call instructions", f"history: last {b['history_messages']} messages, ≤ {history:,} chars",
        f"channel context (new thread only): ≤ {b['channel_chars']:,}", "linked messages: ≤ 3 links, ≤ 20,000",
        f"session: summary ≤ {b['summary_chars']:,}, last {b['max_decisions']} decisions,", "   sticky machine/workspace/repo/branch",
        "workers: status + last result (no report)", "trigger.results: full WorkerResult + report", "machines (live load), repositories, notes",
        f"measured decide prompt: mean {number(parent.get('mean'))},", f"max {number(parent.get('max'))} characters"],
        fill=FILL["parent"])
    panel(axes, 8.0, 3.5, 2.8, 2.9, "Action (validated)", [
        f"reply ≤ {b['reply_chars']:,} characters", f"   overflow → details file ≤ {b['details_chars']:,}",
        f"summary ≤ {b['summary_chars']:,} (rolling)", "decisions (this turn only)", f"delegate: brief ≤ {b['brief_chars']:,}",
        "context · worker_control · note"], fill=FILL["parent"])
    box(axes, 8.0, 1.3, 2.8, 1.4, "Slack thread", f"reply mean {number(n['reply_chars']['mean'])} chars\n"
        f"report mean {number(n['report_chars']['mean'])} chars", fill=FILL["slack"])
    arrow(axes, (1.8, 4.2), (1.8, 3.6), "compacts", color=NAVY, offset=(0.55, 0))
    arrow(axes, (3.4, 2.6), (3.9, 2.6), color=NAVY)
    arrow(axes, (7.5, 4.9), (8.0, 4.9), color=NAVY)
    arrow(axes, (9.4, 3.5), (9.4, 2.7), "outbox", color=NAVY, offset=(0.45, 0))
    arrow(axes, (9.4, 6.45), (1.8, 6.45), "brief (self-contained job)", color=TEAL, offset=(0, 0.5), curve=0.08)
    arrow(axes, (1.8, 1.3), (8.0, 1.6), f"fast path: {n['fast_path_reports']} of {n['post_kinds'].get('report', 0)} "
          "reports posted without a parent call", color=ORANGE, offset=(0.4, -0.55), curve=0.18)
    arrow(axes, (8.0, 2.0), (7.5, 2.0), color=GRAY)
    axes.text(7.75, 2.25, "history", fontsize=7.5, color=GRAY, ha="center")
    return save(figure, "f5_context.png")


# ----- F6: security trust zones -----

def security(s: dict) -> str:
    figure, axes = canvas(11, 7.4)
    box(axes, 0.2, 5.9, 3.3, 1.2, "Untrusted input", "Slack messages, links,\npeer agents, worker output", fill=FILL["danger"],
        edge=RED)
    box(axes, 3.9, 3.2, 6.9, 4.0, "", fill="#f7f9fc", edge=NAVY, dashed=True)
    axes.text(7.35, 6.95, "Fridica daemon (owner's account, local machine)", ha="center", fontsize=10.5, fontweight="bold",
              color=NAVY)
    box(axes, 4.2, 5.4, 3.1, 1.1, "Parent agent", "no tools · scrubbed env\nuntrusted-data notes", fill=FILL["parent"])
    box(axes, 7.5, 5.4, 3.0, 1.1, "Slack tokens", "only in the daemon;\nremoved from every child env", fill=FILL["store"])
    box(axes, 4.2, 3.5, 3.1, 1.3, "Policy & approvals", "delegate_channels · selectors\nauto reviewer · rules · timeout")
    box(axes, 7.5, 3.5, 3.0, 1.3, "Control API + dashboard", "socket 0600 · 127.0.0.1\norigin check · per-run key")
    box(axes, 3.9, 0.2, 6.9, 2.5, "", fill=FILL["worker"], edge=ORANGE)
    axes.text(7.35, 2.42, "Workers on machines (local, or remote over SSH)", ha="center", fontsize=10, fontweight="bold",
              color=ORANGE)
    box(axes, 4.15, 0.4, 3.15, 1.7, "Backend sandbox", "read-only / write / full\nnetwork allowlist (Claude)\n"
        "Codex: all-or-nothing", fill="white", edge=ORANGE, size=9)
    box(axes, 7.5, 0.4, 3.05, 1.7, "gpu_confine (bwrap)", "only own workspace writable\nsettings read-only\n"
        "shares host network", fill="white", edge=ORANGE, size=9)
    box(axes, 0.2, 3.5, 3.3, 1.5, "Artifacts back", "inside own workspace (realpath)\nPNG/PDF/MD magic bytes\n≤3 files, ≤20 MB",
        fill=FILL["store"], size=9)
    box(axes, 0.2, 0.6, 3.3, 1.6, "Remote transport", "ssh -T BatchMode, ControlMaster\nwatchdog: agent dies\nwith its channel",
        fill=FILL["machine"], size=9)
    arrow(axes, (3.5, 6.5), (4.2, 5.95), "data, never\ninstructions", color=RED, offset=(0, 0.35))
    arrow(axes, (5.75, 5.4), (5.75, 4.8), color=GRAY)
    arrow(axes, (5.75, 3.5), (5.75, 2.7), "briefs", color=GRAY, offset=(0.45, 0))
    arrow(axes, (3.9, 1.9), (2.4, 3.5), "files", color=GRAY, offset=(-0.2, 0))
    arrow(axes, (3.5, 1.4), (3.9, 1.4), color=GRAY, both=True)
    return save(figure, "f6_security.png")


# ----- F7: slots, subfolders, GPUs -----

def slots(s: dict) -> str:
    n = s["new"]
    gpu_machines = [machine for machine in s["machines"] if machine["gpus"]] or [
        {"name": "gpu-host", "gpus": [0, 1], "gpu_type": "GPU", "max_jobs": 2, "subfolders": True}]
    figure, axes = canvas(11, 4.6)
    width = 10.9 / len(gpu_machines)
    for index, machine in enumerate(gpu_machines):
        x = 0.3 + index * width
        box(axes, x, 0.3, width - 0.35, 3.9, "", fill=FILL["machine"], edge=NAVY)
        peak = n["peak_by_machine"].get(machine["name"], 0)
        axes.text(x + (width - 0.35) / 2, 3.93, machine["name"], ha="center", fontsize=11, fontweight="bold", color=NAVY)
        axes.text(x + (width - 0.35) / 2, 3.66, f"{len(machine['gpus'])} × {machine['gpu_type'] or 'GPU'} · max_jobs "
                  f"{machine['max_jobs']} · observed peak {peak} concurrent", ha="center", fontsize=8.5, color="#333")
        slot_width = (width - 0.6) / machine["max_jobs"]
        for slot in range(1, machine["max_jobs"] + 1):
            share = slot_share(machine["gpus"], slot, machine["max_jobs"])
            folder = f"<workspace>/worker{slot}" if machine["subfolders"] else "<workspace> (shared)"
            box(axes, x + 0.12 + (slot - 1) * slot_width, 0.55, slot_width - 0.2, 2.8, f"slot {slot}",
                f"{folder}\nCUDA_VISIBLE_DEVICES={','.join(map(str, share))}\none job at a time\nsticky workers",
                fill="white", size=9)
    axes.text(5.5, 0.05, "A worker keeps its slot for life: its directory holds its backend session, so follow-ups resume "
              "in place.", ha="center", fontsize=8.5, color=GRAY)
    return save(figure, "f7_slots.png")


def slot_share(gpus: list[int], slot: int, slots: int) -> list[int]:
    from fridica.machines.registry import slot_gpus
    return list(slot_gpus(tuple(gpus), slot, slots) or ())


# ----- F8: legacy vs overhaul -----

def comparison(s: dict) -> str:
    figure, axes = canvas(11, 6.2)
    axes.text(2.7, 5.9, "Legacy (PR #1–#21)", ha="center", fontsize=12, fontweight="bold", color=GRAY)
    axes.text(8.3, 5.9, "Overhaul (PR #22–)", ha="center", fontsize=12, fontweight="bold", color=NAVY)
    box(axes, 0.3, 4.7, 4.8, 0.8, "Slack events → events table", "inbound log + queue + outbox in one table", fill=FILL["slack"],
        edge=GRAY, size=9)
    box(axes, 0.3, 3.3, 4.8, 1.0, "Replica (one global lock)", "gate, budget, decide, respond, deliver,\nheavy tasks, digests, reload",
        fill=FILL["danger"], edge=RED, size=9)
    box(axes, 0.3, 1.9, 4.8, 1.0, "One CLI run per turn (with tools)", "one session per thread · primary host", fill=FILL["store"],
        edge=GRAY, size=9)
    box(axes, 0.3, 0.5, 4.8, 1.0, "≤ 1 heavy worker per thread", "host from `host:/path` roots · prose report", fill=FILL["worker"],
        edge=GRAY, size=9)
    for y in (4.7, 3.3, 1.9):
        arrow(axes, (2.7, y), (2.7, y - 0.4))
    box(axes, 5.9, 4.7, 4.8, 0.8, "Messages → inbox; posts → outbox", "separate durable queues, idempotent posts",
        fill=FILL["slack"], size=9)
    box(axes, 5.9, 3.3, 4.8, 1.0, "One actor per thread", "pure policy · atomic commits · no global lock", fill=FILL["daemon"], size=9)
    box(axes, 5.9, 1.9, 4.8, 1.0, "Tool-less parent", "triage/decide · validated actions · 1 repair", fill=FILL["parent"], size=9)
    box(axes, 5.9, 0.5, 4.8, 1.0, "Many workers per thread", "machine registry · roles · fan-out/join ·\nslots/GPUs · WorkerResult",
        fill=FILL["worker"], size=9)
    for y in (4.7, 3.3, 1.9):
        arrow(axes, (8.3, y), (8.3, y - 0.4))
    return save(figure, "f8_comparison.png")


# ----- F9: the mount namespace bubblewrap builds for gpu_confine -----

def mounts(s: dict) -> str:
    """Fridica's confinement as layers: later bind mounts cover earlier ones, as in bwrap's argv order."""
    figure, axes = canvas(11, 6.4)
    axes.text(5.5, 6.15, "What a confined worker sees: bwrap applies its argv in order, later mounts cover earlier ones",
              ha="center", fontsize=10.5, color=NAVY)
    layers = [
        ("--ro-bind / /", "the host's whole filesystem, read-only (EROFS on any write)", FILL["store"], "read-only"),
        ("--dev-bind /dev /dev", "real device nodes, so /dev/nvidia* reach CUDA", FILL["machine"], "devices"),
        ("--proc /proc", "a fresh procfs for the new PID namespace: only the sandbox's processes", FILL["daemon"], "private"),
        ("--tmpfs /tmp", "an empty, private /tmp in memory", FILL["daemon"], "private"),
        ("--bind <workspace>/worker<k>", "the slot's own folder, read-write: the only project files it can change",
         FILL["parent"], "read-write"),
        ("--bind-try ~/.claude ~/.codex ~/.claude.json", "backend state (sessions, auth), read-write", FILL["worker"],
         "read-write"),
        ("--ro-bind-try settings.json, config.toml, hooks/", "settings and hooks, read-only again on top",
         FILL["danger"], "read-only"),
    ]
    for index, (flag, meaning, fill, access) in enumerate(layers):
        y = 0.55 + index * 0.72
        indent = 0.25 * index
        axes.add_patch(FancyBboxPatch((0.4 + indent, y), 7.2 - indent, 0.56, boxstyle="round,pad=0.02,rounding_size=0.06",
                                      facecolor=fill, edgecolor=NAVY, linewidth=1.1))
        axes.text(0.55 + indent, y + 0.37, flag, fontsize=8.6, fontweight="bold", color=NAVY, family="monospace")
        axes.text(0.55 + indent, y + 0.14, meaning, fontsize=7.8, color="#222")
        axes.text(7.75, y + 0.28, access, fontsize=8.2, color=RED if access == "read-only" else TEAL, va="center")
    arrow(axes, (9.2, 0.6), (9.2, 5.5), "applied in order", color=GRAY, offset=(0.0, 0.0))
    box(axes, 8.7, 0.55, 2.1, 1.3, "namespaces", "user · mount · pid\nnet, ipc, uts shared", size=9)
    box(axes, 8.7, 4.3, 2.1, 1.25, "process", "no_new_privs · no caps\ndies with its parent", size=9)
    return save(figure, "f9_mounts.png")


# ----- F10: Claude's sandbox network path through socat -----

def socat_bridge(s: dict) -> str:
    figure, axes = canvas(11, 4.8)
    axes.add_patch(FancyBboxPatch((0.2, 0.3), 5.6, 3.9, boxstyle="round,pad=0.02,rounding_size=0.08",
                                  facecolor="#f7f9fc", edgecolor=ORANGE, linewidth=1.4, linestyle="--"))
    axes.text(3.0, 3.95, "sandbox: own network namespace (only loopback)", ha="center", fontsize=9.5, color=ORANGE,
              fontweight="bold")
    box(axes, 0.45, 2.55, 2.2, 1.0, "agent command", "HTTP(S)_PROXY =\nlocalhost:3128 / :1080", fill=FILL["worker"], size=9)
    box(axes, 3.3, 2.55, 2.25, 1.0, "socat (inside)", "TCP-LISTEN:3128\n→ UNIX-CONNECT", size=9)
    box(axes, 0.45, 0.6, 2.2, 1.1, "seccomp filter", "socket(AF_UNIX) → EPERM\nfor the command", fill=FILL["danger"], size=9)
    box(axes, 3.3, 0.6, 2.25, 1.1, "bind-mounted file", "/tmp/claude-http-<id>.sock\n(the only way out)", fill=FILL["store"],
        size=9)
    axes.add_patch(FancyBboxPatch((6.3, 0.3), 4.5, 3.9, boxstyle="round,pad=0.02,rounding_size=0.08",
                                  facecolor="#f7f9fc", edgecolor=NAVY, linewidth=1.4, linestyle="--"))
    axes.text(8.55, 3.95, "host: Claude Code process", ha="center", fontsize=9.5, color=NAVY, fontweight="bold")
    box(axes, 6.55, 2.55, 1.9, 1.0, "socat (host)", "UNIX-LISTEN\n→ TCP:localhost:port", size=9)
    box(axes, 8.7, 2.55, 1.9, 1.0, "proxy", "HTTP + SOCKS\ndomain allowlist", fill=FILL["parent"], size=9)
    box(axes, 7.6, 0.6, 2.2, 1.1, "internet", "allowed domains only\n(none for Fridica by default)", fill=FILL["slack"], size=9)
    arrow(axes, (2.65, 3.05), (3.3, 3.05), color=NAVY)
    arrow(axes, (4.4, 2.55), (4.4, 1.7), color=NAVY)
    arrow(axes, (5.55, 1.15), (6.55, 2.7), color=NAVY, curve=-0.2)
    arrow(axes, (8.45, 3.05), (8.7, 3.05), color=NAVY)
    arrow(axes, (9.65, 2.55), (9.1, 1.7), color=NAVY)
    arrow(axes, (1.55, 1.7), (1.55, 2.55), "blocks direct\nUnix sockets", color=RED, offset=(0.75, 0.0))
    return save(figure, "f10_socat.png")


def draw_all(s: dict) -> list[str]:
    return [architecture(s), slack_mapping(s), layers(s), funnel(s), context_flow(s), security(s), slots(s),
            comparison(s), mounts(s), socat_bridge(s)]
