"use strict";
// Fridica dashboard: every view renders from the daemon's control API through /api/.

const state = { view: "overview", thread: null, key: "" };

function readKey() {
  const match = location.hash.match(/key=([^&]+)/);
  if (match) {
    try { sessionStorage.setItem("fridica-key", match[1]); } catch (error) { /* storage may be blocked */ }
    history.replaceState(null, "", location.pathname);
    return match[1];
  }
  try { return sessionStorage.getItem("fridica-key") || ""; } catch (error) { return ""; }
}

async function api(method, path, body) {
  const options = { method, headers: { Authorization: "Bearer " + state.key } };
  if (body !== undefined) {
    options.headers["Content-Type"] = "application/json";
    options.body = JSON.stringify(body);
  }
  const response = await fetch("/api" + path, options);
  if (response.status === 401) { showUnlock(); throw new Error("locked"); }
  const data = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(data.error || response.statusText);
  return data;
}

function el(tag, attributes = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (name === "onclick") node.addEventListener("click", value);
    else if (name === "class") node.className = value;
    else node.setAttribute(name, value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function when(seconds) {
  if (!seconds) return "";
  return new Date(seconds * 1000).toLocaleString();
}

function ago(seconds) {
  if (!seconds) return "";
  const delta = Math.max(0, Date.now() / 1000 - seconds);
  if (delta < 90) return Math.round(delta) + "s ago";
  if (delta < 5400) return Math.round(delta / 60) + "m ago";
  if (delta < 129600) return Math.round(delta / 3600) + "h ago";
  return Math.round(delta / 86400) + "d ago";
}

function table(headers, rows) {
  return el("div", { class: "table-wrap" }, el("table", {},
    el("thead", {}, el("tr", {}, headers.map((header) => el("th", {}, header)))),
    el("tbody", {}, rows)));
}

function button(label, handler, extra = "") {
  return el("button", { class: extra, onclick: async (event) => {
    event.stopPropagation();
    event.target.disabled = true;
    try { await handler(); } catch (error) { alert(error.message); }
    await refresh();
  } }, label);
}

function statusPill(value) {
  const tone = { complete: "ok", done: "ok", idle: "ok", active: "ok", waiting: "warn", working: "warn", running: "warn",
    queued: "warn", awaiting_approval: "warn", paused: "warn", blocked: "bad", failed: "bad", lost: "bad", ambiguous: "bad" }[value] || "";
  return el("span", { class: "pill " + tone }, value);
}

const views = {
  async overview() {
    const [status, approvals, outbox] = await Promise.all([api("GET", "/status"), api("GET", "/approvals"), api("GET", "/outbox")]);
    const cards = [["Running jobs", status.running_jobs], ["Queued jobs", status.queued_jobs],
      ["Pending approvals", approvals.length], ["Problem posts", outbox.length]];
    return [
      el("div", { class: "grid" }, cards.map(([label, value]) => el("div", { class: "card" }, el("div", { class: "muted" }, label), el("div", { class: "big" }, value)))),
      el("p", { class: "muted" }, `Owner ${status.owner} · started ${when(status.started_at)}` + (status.observe_only ? " · observe-only" : "")),
      approvals.length ? [el("h2", {}, "Needs your decision"), approvalTable(approvals)] : null,
      outbox.length ? [el("h2", {}, "Posts that did not go out"), outboxTable(outbox)] : null,
    ];
  },

  async threads() {
    if (state.thread) return threadDetail(state.thread);
    const threads = await api("GET", "/threads");
    return table(["Thread", "Status", "Turns", "Context", "Summary", "Updated"], threads.map((thread) =>
      el("tr", { class: "link", onclick: () => { state.thread = thread.id; refresh(); } },
        el("td", {}, `#${thread.key.channel} · ${thread.key.root_ts}`),
        el("td", {}, statusPill(thread.control === "active" ? thread.status : thread.control)),
        el("td", {}, thread.turns),
        el("td", {}, [thread.context.machine, thread.context.workspace, thread.context.branch].filter(Boolean).join(" · ")),
        el("td", {}, (thread.summary || "").slice(0, 140)),
        el("td", {}, ago(thread.updated)))));
  },

  async workers() {
    return workerTable(await api("GET", "/workers"));
  },

  async approvals() {
    const approvals = await api("GET", "/approvals");
    return approvals.length ? approvalTable(approvals) : el("p", { class: "muted" }, "Nothing is waiting for approval.");
  },

  async machines() {
    const machines = await api("GET", "/machines");
    return table(["Machine", "Transport", "Tags", "Workspaces", "Backends", "Jobs", "Processes"], machines.map((machine) => el("tr", {},
      el("td", {}, machine.name), el("td", {}, machine.transport + (machine.host ? " · " + machine.host : "")),
      el("td", {}, machine.tags.map((tag) => el("span", { class: "chip" }, tag))),
      el("td", {}, Object.entries(machine.workspaces).map(([name, mode]) => `${name} (${mode})`).join(", ")),
      el("td", {}, machine.backends.join(", ")),
      el("td", {}, `${machine.busy_jobs} / ${machine.max_jobs}`),
      el("td", {}, `${machine.live_workers} / ${machine.max_workers}`))));
  },

  async activity() {
    const entries = await api("GET", "/activity");
    return table(["When", "Who", "What", "Target"], entries.map((entry) => el("tr", {},
      el("td", {}, when(entry.time)), el("td", {}, entry.actor), el("td", {}, entry.action), el("td", {}, entry.target))));
  },
};

async function threadDetail(id) {
  const detail = await api("GET", "/threads/" + encodeURIComponent(id));
  const session = detail.session;
  const act = (action) => () => api("POST", `/threads/${encodeURIComponent(id)}/${action}`, {});
  return [
    el("div", { class: "actions" }, button("← All threads", async () => { state.thread = null; }),
      button("Resume", act("resume"), "primary"), button("Pause", act("pause")), button("Close", act("close")),
      button("Archive", act("archive")), button("Clean", act("clean"), "danger")),
    el("p", {}, statusPill(session.control === "active" ? session.status : session.control), " ",
      `${session.turns} turns`, session.pause_reason ? " · " + session.pause_reason : ""),
    session.summary ? [el("h2", {}, "Summary"), el("pre", {}, session.summary)] : null,
    session.decisions.length ? [el("h2", {}, "Decisions"), el("ul", {}, session.decisions.map((item) => el("li", {}, item)))] : null,
    el("h2", {}, "Messages"),
    el("div", { class: "messages" }, detail.messages.map((message) =>
      el("div", { class: "message" + (message.source === "self" ? " self" : "") },
        el("div", { class: "muted" }, `${message.sender} · ${when(Number(message.ts))}`), message.text))),
    detail.workers.length ? [el("h2", {}, "Workers"), workerTable(detail.workers)] : null,
    detail.jobs.length ? [el("h2", {}, "Jobs"), table(["Job", "Worker", "Status", "Brief", "Result"], detail.jobs.map((job) => el("tr", {},
      el("td", {}, job.id), el("td", {}, job.worker_id), el("td", {}, statusPill(job.status)),
      el("td", {}, job.brief.slice(0, 200)),
      el("td", {}, job.result ? job.result.summary : job.error))))] : null,
  ];
}

function workerTable(workers) {
  return table(["Worker", "Machine", "Backend", "Role", "Status", "Summary", ""], workers.map((worker) => el("tr", {},
    el("td", {}, worker.id), el("td", {}, `${worker.machine} · ${worker.workspace}`), el("td", {}, worker.backend),
    el("td", {}, worker.role + (worker.ephemeral ? " (one job)" : "")), el("td", {}, statusPill(worker.status)),
    el("td", {}, (worker.summary || "").slice(0, 160)),
    el("td", {}, worker.status === "stopped" ? "" : el("div", { class: "actions" },
      button("Interrupt", () => api("POST", `/workers/${worker.id}/interrupt`, {})),
      button("Stop", () => api("POST", `/workers/${worker.id}/stop`, {}), "danger"))))));
}

function approvalTable(approvals) {
  return table(["Requested", "Worker", "Request", ""], approvals.map((approval) => el("tr", {},
    el("td", {}, ago(approval.created)), el("td", {}, approval.worker_id),
    el("td", {}, el("div", {}, approval.summary), approval.detail.reason ? el("div", { class: "muted" }, approval.detail.reason) : null),
    el("td", {}, el("div", { class: "actions" },
      button("Allow once", () => api("POST", `/approvals/${approval.id}`, { decision: "once" }), "primary"),
      button("Allow for session", () => api("POST", `/approvals/${approval.id}`, { decision: "session" })),
      button("Deny", () => api("POST", `/approvals/${approval.id}`, { decision: "deny" }), "danger"))))));
}

function outboxTable(items) {
  return table(["Kind", "State", "Text", "Error", ""], items.map((item) => el("tr", {},
    el("td", {}, item.kind), el("td", {}, statusPill(item.state)), el("td", {}, (item.text || item.filename).slice(0, 140)),
    el("td", {}, item.error),
    el("td", {}, item.state === "blocked" ? "" : button("Retry", () => api("POST", `/outbox/${item.id}/retry`, {}))))));
}

function showUnlock() {
  document.getElementById("unlock").hidden = false;
}

async function refresh() {
  const view = document.getElementById("view");
  try {
    const [status, approvals] = await Promise.all([api("GET", "/status"), api("GET", "/approvals")]);
    const pill = document.getElementById("status");
    pill.textContent = status.slack;
    pill.className = "pill " + (status.slack === "connected" ? "ok" : "warn");
    document.getElementById("approval-count").textContent = approvals.length || "";
    view.replaceChildren(...[await views[state.view]()].flat(3).filter(Boolean));
  } catch (error) {
    if (error.message !== "locked") {
      document.getElementById("status").textContent = "offline";
      document.getElementById("status").className = "pill bad";
      view.replaceChildren(el("p", { class: "muted" }, error.message));
    }
  }
}

function start() {
  state.key = readKey();
  if (!state.key) showUnlock();
  document.getElementById("unlock-form").addEventListener("submit", (event) => {
    event.preventDefault();
    state.key = document.getElementById("key").value.trim();
    try { sessionStorage.setItem("fridica-key", state.key); } catch (error) { /* storage may be blocked */ }
    document.getElementById("unlock").hidden = true;
    refresh();
  });
  for (const tab of document.querySelectorAll("#tabs button")) {
    tab.addEventListener("click", () => {
      state.view = tab.dataset.view;
      state.thread = null;
      for (const other of document.querySelectorAll("#tabs button")) other.classList.toggle("active", other === tab);
      refresh();
    });
  }
  refresh();
  setInterval(() => { if (!document.hidden) refresh(); }, 5000);
}

if (typeof document !== "undefined") start();
if (typeof module !== "undefined") module.exports = { el, ago, statusPill, views, state, api };
