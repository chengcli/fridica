"""Data charts for the design document (matplotlib, one consistent style)."""

from __future__ import annotations

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

from diagrams import BLUE, GRAY, NAVY, ORANGE, RED, TEAL, save  # noqa: E402

PALETTE = [NAVY, TEAL, ORANGE, BLUE, GRAY, RED, "#8e7cc3", "#b5a642"]


def style(axes, title: str = "", xlabel: str = "", ylabel: str = "") -> None:
    axes.spines[["top", "right"]].set_visible(False)
    axes.grid(axis="y", color="#e5e5e5", linewidth=0.8)
    axes.set_axisbelow(True)
    if title:
        axes.set_title(title, fontsize=10.5, color=NAVY)
    axes.set_xlabel(xlabel)
    axes.set_ylabel(ylabel)


def bars(axes, labels, values, color=NAVY, annotate=True, rotation=0):
    positions = range(len(labels))
    axes.bar(positions, values, color=color, width=0.65)
    axes.set_xticks(list(positions), labels, rotation=rotation, ha="right" if rotation else "center", fontsize=8.5)
    if annotate:
        for position, value in zip(positions, values):
            axes.text(position, value, f"{value:g}", ha="center", va="bottom", fontsize=8)


def activity(s: dict) -> str:
    """Messages per hour by source (current design)."""
    n = s["new"]
    figure, axes = plt.subplots(figsize=(10, 3.4))
    hours = list(range(n["hours"]))
    bottom = [0] * len(hours)
    for index, source in enumerate(("socket", "catchup", "self")):
        counts = n["hourly_by_source"].get(source, {})
        values = [counts.get(hour, 0) for hour in hours]
        label = {"socket": "live (Socket Mode)", "catchup": "catch-up", "self": "Fridica's own posts"}[source]
        axes.bar(hours, values, bottom=bottom, color=[BLUE, ORANGE, TEAL][index], label=label, width=0.85)
        bottom = [b + v for b, v in zip(bottom, values)]
    style(axes, "Messages per hour since the overhaul went live", "hours since first message", "messages")
    axes.legend(frameon=False, fontsize=8.5)
    return save(figure, "c1_activity.png")


def senders(s: dict) -> str:
    n = s["new"]
    figure, axes = plt.subplots(figsize=(6.4, 3.2))
    labels, values = zip(*n["senders"].items())
    bars(axes, labels, values, color=[TEAL if "Fridica" in label else ORANGE if "peer" in label else NAVY for label in labels],
         rotation=20)
    style(axes, "Who wrote the messages (anonymized)", "", "messages")
    return save(figure, "c2_senders.png")


def parent_calls(s: dict) -> str:
    n = s["new"]
    calls = [name for name in ("triage", "decide", "repair", "debrief") if name in n["parent"]]
    figure, (left, right) = plt.subplots(1, 2, figsize=(10, 3.6))
    left.boxplot([n["parent"][name]["latencies"] for name in calls], showfliers=True, widths=0.5)
    left.set_xticks(range(1, len(calls) + 1), [f"{name}\n(n={n['parent'][name]['count']})" for name in calls])
    style(left, "Parent call latency", "", "seconds")
    right.boxplot([[value / 1000 for value in n["parent"][name]["prompts"]] for name in calls], widths=0.5)
    right.set_xticks(range(1, len(calls) + 1), calls)
    style(right, "Parent prompt size", "", "thousand characters")
    figure.tight_layout()
    return save(figure, "c3_parent_calls.png")


def context_sizes(s: dict) -> str:
    """Measured sizes at each layer of the compaction hierarchy, against their caps (log scale)."""
    n, budgets = s["new"], s["budgets"]
    items = [
        ("decide prompt\n(parent input)", n["parent"].get("decide", {}).get("prompt_chars", {}), None),
        ("triage prompt", n["parent"].get("triage", {}).get("prompt_chars", {}), None),
        ("WorkerResult.report", n["result_report_chars"], budgets["result_report"]),
        ("WorkerResult.summary", n["result_summary_chars"], budgets["result_summary"]),
        ("thread summary", n["summary_chars"], budgets["summary_chars"]),
        ("Slack report post", n["report_chars"], None),
        ("Slack reply", n["reply_chars"], budgets["reply_chars"]),
    ]
    figure, axes = plt.subplots(figsize=(10, 4.2))
    labels = [label for label, _, _ in items]
    means = [values.get("mean") or 0 for _, values, _ in items]
    p95 = [values.get("p95") or 0 for _, values, _ in items]
    positions = range(len(items))
    axes.barh(positions, p95, color="#c9d6ea", height=0.6, label="p95")
    axes.barh(positions, means, color=NAVY, height=0.35, label="mean")
    for position, (_, values, cap) in zip(positions, items):
        if cap:
            axes.plot([cap, cap], [position - 0.35, position + 0.35], color=RED, linewidth=2)
        right = max(values.get("p95") or 0, cap or 0)
        axes.text(right * 1.08, position, f"mean {values.get('mean') or 0:,.0f}", va="center", fontsize=8)
    axes.plot([], [], color=RED, linewidth=2, label="cap")
    axes.set_xscale("log")
    axes.set_xlim(left=200)
    axes.set_yticks(list(positions), labels, fontsize=8.5)
    axes.invert_yaxis()
    style(axes, "Characters at each layer: large inputs are compacted before reaching Slack", "characters (log scale)")
    axes.grid(axis="x", color="#e5e5e5")
    axes.legend(frameon=False, fontsize=8.5, loc="lower right")
    return save(figure, "c4_context_sizes.png")


def jobs(s: dict) -> str:
    n = s["new"]
    figure, axes = plt.subplots(2, 2, figsize=(10, 6.6))
    machines = sorted(n["jobs_by_machine_status"])
    statuses = ["done", "interrupted", "failed", "cancelled", "running", "queued"]
    bottom = [0] * len(machines)
    for index, status in enumerate(statuses):
        values = [n["jobs_by_machine_status"][machine].get(status, 0) for machine in machines]
        if any(values):
            axes[0][0].bar(machines, values, bottom=bottom, color=PALETTE[index], label=status, width=0.6)
            bottom = [b + v for b, v in zip(bottom, values)]
    style(axes[0][0], "Jobs by machine and outcome", "", "jobs")
    axes[0][0].legend(frameon=False, fontsize=8)
    roles = list(n["jobs_by_role"])
    bars(axes[0][1], roles, [n["jobs_by_role"][role] for role in roles], color=TEAL)
    style(axes[0][1], "Jobs by worker role", "", "jobs")
    minutes = n["job_minutes_all"]
    edges = [0, 1, 2, 5, 10, 20, 30, 60, float("inf")]
    labels = ["<1", "1–2", "2–5", "5–10", "10–20", "20–30", "30–60", ">60"]
    counts = [sum(1 for value in minutes if low <= value < high) for low, high in zip(edges, edges[1:])]
    bars(axes[1][0], labels, counts, color=NAVY)
    style(axes[1][0], f"Job duration (done, n={len(minutes)}; median {n['job_minutes']['p50'] or 0:.1f} min)", "minutes",
          "jobs")
    sizes = n["group_sizes"]
    bars(axes[1][1], [f"{size} job{'s' if size > 1 else ''}" for size in sizes], list(sizes.values()), color=ORANGE)
    style(axes[1][1], "Jobs per parent turn (fan-out groups)", "", "turns")
    figure.tight_layout()
    return save(figure, "c5_jobs.png")


def concurrency(s: dict) -> str:
    n = s["new"]
    figure, axes = plt.subplots(figsize=(10, 2.8))
    curve = n["concurrency_curve"]
    if curve:
        times = [stamp / 3600 for stamp, _ in curve]
        values = [running for _, running in curve]
        axes.step(times, values, where="post", color=NAVY)
        axes.fill_between(times, values, step="post", color="#c9d6ea")
    style(axes, f"Jobs running at once (peak {n['peak_concurrency']}; global max_jobs "
          f"{s['defaults']['limits']['max_jobs']})", "hours since first message", "running jobs")
    return save(figure, "c6_concurrency.png")


def threads(s: dict) -> str:
    n, old = s["new"], s["legacy"]
    figure, (left, right) = plt.subplots(1, 2, figsize=(10, 3.5))
    per = n["workers_per_thread"]
    bars(left, [str(key) for key in per], list(per.values()), color=TEAL)
    style(left, "Workers per thread (threads that delegated)", "workers", "threads")
    turns_new = n["turns"]
    turns_old = old["turns"] if old else {}
    keys = sorted(set(map(int, turns_new)) | set(map(int, turns_old)))
    width = 0.4
    right.bar([key - width / 2 for key in keys], [turns_old.get(key, turns_old.get(str(key), 0)) for key in keys],
              width=width, color=GRAY, label="legacy")
    right.bar([key + width / 2 for key in keys], [turns_new.get(key, turns_new.get(str(key), 0)) for key in keys],
              width=width, color=NAVY, label="overhaul")
    style(right, "Replies per thread (turns)", "turns", "threads")
    right.legend(frameon=False, fontsize=8.5)
    figure.tight_layout()
    return save(figure, "c7_threads.png")


def replies(s: dict) -> str:
    n = s["new"]
    figure, axes = plt.subplots(figsize=(10, 3.0))
    axes.hist(n["reply_lengths"], bins=30, color=NAVY, edgecolor="white")
    axes.axvline(s["budgets"]["reply_chars"], color=RED, linewidth=1.5)
    axes.text(s["budgets"]["reply_chars"], axes.get_ylim()[1] * 0.9, " reply cap", color=RED, fontsize=8)
    style(axes, "Length of Slack replies and reports", "characters", "posts")
    return save(figure, "c8_replies.png")


def legacy_comparison(s: dict) -> str:
    n, old = s["new"], s["legacy"]
    if not old:
        return ""
    figure, (left, right) = plt.subplots(1, 2, figsize=(10, 3.8))
    per_day_new = {"messages in": n["ingested"] / max(n["days"], 1e-9), "posts out": n["posts_text"] / max(n["days"], 1e-9),
                   "worker jobs": n["jobs"] / max(n["days"], 1e-9)}
    per_day_old = {"messages in": old["inbound"] / max(old["days"], 1e-9), "posts out": old["own_posts"] / max(old["days"], 1e-9),
                   "worker jobs": old["heavy_jobs"] / max(old["days"], 1e-9)}
    keys = list(per_day_new)
    width = 0.38
    left.bar([index - width / 2 for index in range(len(keys))], [per_day_old[key] for key in keys], width, color=GRAY,
             label="legacy")
    left.bar([index + width / 2 for index in range(len(keys))], [per_day_new[key] for key in keys], width, color=NAVY,
             label="overhaul")
    left.set_xticks(range(len(keys)), keys)
    for index, key in enumerate(keys):
        left.text(index - width / 2, per_day_old[key], f"{per_day_old[key]:.0f}", ha="center", va="bottom", fontsize=8)
        left.text(index + width / 2, per_day_new[key], f"{per_day_new[key]:.0f}", ha="center", va="bottom", fontsize=8)
    style(left, "Throughput per day", "", "per day")
    left.legend(frameon=False, fontsize=8.5)
    # Legacy values are structural limits (one worker per thread, no fan-out); its job end times were not
    # recorded, so its concurrency cannot be measured and is not shown.
    structure = {"workers per\nthread (max)": (1, n["max_workers_in_thread"]),
                 "fan-out turns\n(>1 job)": (0, n["fan_out_groups"]),
                 "machines\nused": (len(old["heavy_by_host"]), len(n["jobs_by_machine_status"]))}
    keys = list(structure)
    left_values = [structure[key][0] for key in keys]
    right_values = [structure[key][1] for key in keys]
    right.bar([index - width / 2 for index in range(len(keys))], left_values, width, color=GRAY, label="legacy")
    right.bar([index + width / 2 for index in range(len(keys))], right_values, width, color=NAVY, label="overhaul")
    right.set_xticks(range(len(keys)), keys, fontsize=8.5)
    for index in range(len(keys)):
        right.text(index - width / 2, left_values[index], f"{left_values[index]}", ha="center", va="bottom", fontsize=8)
        right.text(index + width / 2, right_values[index], f"{right_values[index]}", ha="center", va="bottom", fontsize=8)
    style(right, "Delegation structure", "", "count")
    figure.tight_layout()
    return save(figure, "c9_legacy_comparison.png")


# (label, probe key, values that mean "confined", values that are intended either way)
SANDBOX_ROWS = [
    ("user namespace", "ns_user", {"private"}, set()),
    ("mount namespace", "ns_mnt", {"private"}, set()),
    ("PID namespace", "ns_pid", {"private"}, set()),
    ("network namespace", "ns_net", {"private"}, set()),
    ("effective capabilities", "cap_eff", {"0000000000000000"}, set()),
    ("no_new_privs", "no_new_privs", {"1"}, set()),
    ("seccomp (2 = filter)", "seccomp", {"2"}, set()),
    ("processes visible", "visible_pids", None, set()),
    ("signal a host process", "signal_host", {"blocked"}, set()),
    ("write the workspace", "write_workspace", set(), {"allowed"}),
    ("write $HOME", "write_home", {"blocked"}, set()),
    ("write backend settings", "write_settings", {"blocked", "absent"}, set()),
    ("write hooks directory", "write_hooks", {"blocked", "absent"}, set()),
    ("see host /tmp", "host_tmp", {"hidden"}, set()),
    ("read system files", "read_system", set(), {"allowed"}),
    ("read $HOME", "read_home", {"blocked"}, set()),
    ("read ~/.ssh", "read_ssh", {"blocked", "absent"}, set()),
    ("device nodes in /dev", "dev_entries", None, set()),
    ("systemd user bus (D-Bus)", "systemd_user_bus", {"unusable"}, set()),
    ("SSH agent", "ssh_agent", {"unreachable", "not_inherited"}, set()),
    ("create Unix sockets", "unix_socket", {"blocked"}, set()),
    ("direct Internet connection", "direct_network", {"blocked"}, set()),
]


def sandbox_matrix(s: dict) -> str | None:
    probe = s.get("sandbox") or {}
    columns = [(name, label) for name, label in (("host", "no sandbox"), ("fridica", "Fridica gpu_confine"),
                                                  ("claude", "Claude sandbox")) if probe.get(name)]
    if not columns:
        return None
    figure, axes = plt.subplots(figsize=(10, 7.4))
    good, bad, neutral = "#cfe8d9", "#f6d0c9", "#e9edf3"
    for row, (label, key, confined, intended) in enumerate(SANDBOX_ROWS):
        for column, (name, _) in enumerate(columns):
            value = probe[name].get(key, "n/a")
            if confined is None:
                number = int(value) if value.isdigit() else -1
                host = int(probe["host"].get(key, "0") or 0)
                color = good if 0 <= number < host / 4 else (neutral if name == "host" else bad)
            elif value in intended:
                color = neutral
            else:
                color = good if value in confined else bad
            shown = "none" if key == "cap_eff" and value == "0000000000000000" else value.replace("_", " ")
            axes.add_patch(plt.Rectangle((column, row), 1, 1, facecolor=color, edgecolor="white", linewidth=2))
            axes.text(column + 0.5, row + 0.5, shown, ha="center", va="center", fontsize=8)
    axes.set_xlim(0, len(columns))
    axes.set_ylim(len(SANDBOX_ROWS), 0)
    axes.set_xticks([index + 0.5 for index in range(len(columns))], [label for _, label in columns], fontsize=9)
    axes.xaxis.tick_top()
    axes.set_yticks([index + 0.5 for index in range(len(SANDBOX_ROWS))], [label for label, *_ in SANDBOX_ROWS],
                    fontsize=8.5)
    axes.tick_params(length=0)
    for spine in axes.spines.values():
        spine.set_visible(False)
    handles = [plt.Rectangle((0, 0), 1, 1, facecolor=color) for color in (good, bad, neutral)]
    axes.legend(handles, ["confined", "not confined", "intended / baseline"], loc="upper center",
                bbox_to_anchor=(0.5, -0.01), ncol=3, frameon=False, fontsize=8.5)
    return save(figure, "c10_sandbox.png")


def draw_all(s: dict) -> list[str]:
    return [name for name in (activity(s), senders(s), parent_calls(s), context_sizes(s), jobs(s), concurrency(s),
                              threads(s), replies(s), legacy_comparison(s), sandbox_matrix(s)) if name]
