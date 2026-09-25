"use strict";

const state = {
  view: "overview", thread: null, key: "", auto: true, data: {},
  query: "", threadFilter: "active", workerFilter: "current", threadLayout: "list", threadLimit: 200,
  refreshing: false, pending: false, instructionDraft: null,
};

function readKey() {
  const match = location.hash.match(/key=([^&]+)/);
  if (match) {
    try { sessionStorage.setItem("fridica-key", match[1]); } catch (_) { /* storage may be blocked */ }
    history.replaceState(null, "", location.pathname);
    return match[1];
  }
  try { return sessionStorage.getItem("fridica-key") || ""; } catch (_) { return ""; }
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
    if (name.startsWith("on")) node.addEventListener(name.slice(2), value);
    else if (name === "class") node.className = value;
    else if (name === "value") node.value = value;
    else node.setAttribute(name, value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function when(seconds) { return seconds ? new Date(seconds * 1000).toLocaleString() : ""; }
function ago(seconds) {
  if (!seconds) return "";
  const delta = Math.max(0, Date.now() / 1000 - seconds);
  if (delta < 90) return Math.round(delta) + "s ago";
  if (delta < 5400) return Math.round(delta / 60) + "m ago";
  if (delta < 129600) return Math.round(delta / 3600) + "h ago";
  return Math.round(delta / 86400) + "d ago";
}

function statusPill(value) {
  const tone = { complete: "ok", done: "ok", idle: "ok", active: "ok", connected: "ok",
    waiting: "warn", working: "warn", running: "warn", queued: "warn", awaiting_approval: "warn",
    paused: "warn", pending: "warn", processing: "warn", processed: "ok", dropped: "bad",
    blocked: "bad", failed: "bad", lost: "bad", ambiguous: "bad" }[value] || "";
  return el("span", { class: "pill " + tone }, (value || "unknown").replaceAll("_", " "));
}

function button(label, handler, extra = "") {
  return el("button", { class: extra, onclick: async (event) => {
    event.stopPropagation();
    event.currentTarget.disabled = true;
    try { await handler(); } catch (error) { toast(error.message, true); }
    finally { event.currentTarget.disabled = false; }
  } }, label);
}

function toast(message, error = false) {
  const node = document.getElementById("toast");
  node.textContent = message;
  node.style.background = error ? "#8d5552" : "#2e4b52";
  node.hidden = false;
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => { node.hidden = true; }, 4500);
}

function confirmAction(title, body, detail = "", dangerous = false) {
  const dialog = document.getElementById("confirm-dialog");
  document.getElementById("dialog-title").textContent = title;
  document.getElementById("dialog-body").textContent = body;
  const extra = document.getElementById("dialog-detail");
  extra.textContent = detail;
  extra.hidden = !detail;
  const submit = document.getElementById("dialog-confirm");
  submit.textContent = dangerous ? "Confirm action" : "Continue";
  submit.className = dangerous ? "danger" : "primary";
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => resolve(dialog.returnValue === "confirm"), { once: true });
    dialog.showModal();
  });
}

async function act(method, path, body, title, explanation, detail = "", dangerous = false) {
  if (!await confirmAction(title, explanation, detail, dangerous)) return false;
  await api(method, path, body);
  toast("Request accepted. Refreshing status…");
  await refresh(true);
  return true;
}

function go(view, thread = null) {
  state.view = view;
  state.thread = thread;
  state.query = "";
  document.getElementById("current-view").textContent = ({ inbox: "Inbox", threads: "Threads", workers: "Workers",
    machines: "Machines & access", activity: "Activity", settings: "Settings" })[view] || "Overview";
  for (const tab of document.querySelectorAll("[data-view]")) tab.classList.toggle("active", tab.dataset.view === view);
  refresh(true);
}

function page(title, description, side = null) {
  return el("div", { class: "page-head" },
    el("div", {}, el("h1", {}, title), el("div", { class: "sub" }, description)),
    side ? el("div", { class: "head-side" }, side) : null);
}

function panel(title, body, action = null) {
  return el("section", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, title), action), body);
}

function empty(message) { return el("div", { class: "empty" }, message); }
function metric(label, value, note, view, tone = "", mark = "↗") {
  return el("button", { class: "metric " + tone, onclick: () => go(view) },
    el("span", { class: "metric-label" }, label, el("span", { class: "metric-mark" }, mark)),
    el("strong", {}, value), el("small", {}, note));
}

function threadTitle(thread) {
  return thread.summary || [thread.context?.repo, thread.context?.workspace].filter(Boolean).join(" · ") || "Slack conversation";
}
function threadContext(thread) {
  return [thread.context?.repo || thread.context?.workspace, thread.context?.branch, ago(thread.updated)].filter(Boolean).join(" · ");
}
function jobAge(job) {
  const started = job.status === "running" ? job.started_at : job.queued_at;
  return started ? `${job.status === "running" ? "Started" : "Queued"} ${ago(started)}` : "";
}
function threadLink(id, label = "Open thread") { return button(label, () => go("threads", id), "section-link"); }
function slackUrl(thread) {
  const key = thread.key;
  return `https://app.slack.com/archives/${encodeURIComponent(key.channel)}/p${key.root_ts.replace(".", "")}`;
}
function attention(data) {
  const threads = data.attentionThreads || data.threads || [];
  return [
    ...(data.approvals || []).map((item) => ({ type: "approval", id: item.id, session_id: item.session_id,
      title: item.summary, note: item.kind + " · requested " + ago(item.created), tone: "bad" })),
    ...threads.filter((item) => item.control === "paused" || item.status === "blocked").map((item) => ({
      type: "thread", id: item.id, session_id: item.id, title: threadTitle(item),
      note: (item.pause_reason || item.status) + " · " + threadContext(item), tone: "warn" })),
    ...(data.outbox || []).map((item) => ({ type: "outbox", id: item.id, session_id: item.session_id,
      title: item.state === "ambiguous" ? "Delivery needs verification" : "Slack delivery failed",
      note: (item.text || item.filename || item.kind).slice(0, 120), tone: "bad" })),
  ];
}
function attentionCard(item) {
  const label = { approval: "Approval", thread: "Paused thread", outbox: "Delivery" }[item.type];
  return el("div", { class: "item-card priority-line " + item.tone },
    el("div", { class: "item-top" }, el("span", { class: "item-title" }, item.title), statusPill(label.toLowerCase())),
    el("p", {}, item.note),
    el("div", { class: "item-meta" }, threadLink(item.session_id, "Review →")));
}

function activeJobRow(job, workers) {
  const worker = workers.find((item) => item.id === job.worker_id);
  const meta = [worker ? `${worker.role} · ${worker.machine}` : "Worker", jobAge(job)].filter(Boolean).join(" · ");
  return el("div", { class: "list-row" },
    el("div", {}, threadLink(job.session_id, job.brief || "Untitled job"), el("p", {}, meta)), statusPill(job.status));
}

function overview(data) {
  const items = attention(data), jobs = data.jobs || [], threads = data.threads || [];
  const paused = (data.attentionThreads || threads.filter((item) => item.control === "paused" || item.status === "blocked")).length;
  const failed = (data.outbox || []).length;
  const running = jobs.filter((item) => item.status === "running").length;
  const queued = jobs.filter((item) => item.status === "queued").length;
  return [page("Overview", "What's running and what needs you."),
    el("div", { class: "stats" },
      metric("Needs approval", data.approvals.length, "Review before work continues", "inbox", data.approvals.length ? "warn" : "", "◇"),
      metric("Active work", jobs.length, `${running} running · ${queued} queued`, "workers", "", "↗"),
      metric("Paused threads", paused, "Ready for your direction", "inbox", paused ? "warn" : "", "Ⅱ"),
      metric("Delivery issues", failed, "Posts to check", "inbox", failed ? "bad" : "", "!")),
    el("div", { class: "overview-grid" },
      panel("Needs your attention", items.length ? items.slice(0, 5).map(attentionCard) : empty("All clear. Nothing needs a decision right now."),
        button("Open inbox →", () => go("inbox"), "section-link")),
      el("div", { class: "stack" },
        panel("Work in progress", jobs.length ? jobs.slice(0, 5).map((job) => activeJobRow(job, data.workers))
          : empty("No jobs are running or queued."),
        button("View workers →", () => go("workers"), "section-link")),
        panel("Recent conversations", threads.length ? threads.slice(0, 5).map((thread) => el("div", { class: "list-row" },
          el("div", {}, threadLink(thread.id, threadTitle(thread)), el("p", {}, threadContext(thread))),
          statusPill(thread.control === "active" ? thread.status : thread.control))) : empty("No conversations yet."),
        button("View all →", () => go("threads"), "section-link"))))];
}

function approvalActions(item) {
  const path = `/approvals/${encodeURIComponent(item.id)}`;
  const detail = JSON.stringify(item.detail || {}, null, 2);
  return el("div", { class: "actions" },
    button("Allow once", () => act("POST", path, { decision: "once" }, "Allow this request once?",
      "The worker can perform this one requested action.", detail), "primary"),
    button("Allow for session", () => act("POST", path, { decision: "session" }, "Allow for this session?",
      "The same kind of request may proceed again during this worker session. Review the scope carefully.", detail)),
    button("Deny", () => act("POST", path, { decision: "deny" }, "Deny this request?",
      "The worker will receive the denial and can continue within its remaining permissions.", detail, true), "danger"));
}
function inbox(data) {
  const paused = data.attentionThreads || data.threads.filter((item) => item.control === "paused" || item.status === "blocked");
  return [page("Inbox", "Requests that need a human decision, plus stalled or undelivered work."),
    panel(`Approvals · ${data.approvals.length}`, data.approvals.length ? data.approvals.map((item) =>
      el("article", { class: "item-card priority-line bad" },
        el("div", { class: "item-top" }, el("strong", {}, item.summary), statusPill(item.kind)),
        el("p", {}, item.detail?.reason || "Review the full request before deciding."),
        el("div", { class: "item-meta" }, ago(item.created), threadLink(item.session_id)), approvalActions(item)))
      : empty("No approvals pending.")),
    panel(`Paused or blocked · ${paused.length}`, paused.length ? paused.map((item) =>
      el("article", { class: "item-card priority-line" },
        el("div", { class: "item-top" }, el("strong", {}, threadTitle(item)), statusPill(item.control === "paused" ? "paused" : "blocked")),
        el("p", {}, item.pause_reason || "This thread needs a review before work resumes."),
        el("div", { class: "item-meta" }, threadContext(item), threadLink(item.id))))
      : empty("No threads are paused or blocked.")),
    panel(`Delivery issues · ${data.outbox.length}`, data.outbox.length ? data.outbox.map((item) =>
      el("article", { class: "item-card priority-line bad" },
        el("div", { class: "item-top" }, el("strong", {}, item.state === "ambiguous" ? "Delivery unconfirmed" : "Delivery failed"), statusPill(item.state)),
        el("p", {}, (item.text || item.filename || item.kind).slice(0, 250)),
        el("div", { class: "item-meta" }, item.error || "", threadLink(item.session_id),
          item.state === "failed" ? button("Retry", () => act("POST", `/outbox/${item.id}/retry`, {}, "Retry this post?",
            "This will send the failed post to Slack again. Confirm it has not already appeared in the thread.", item.text || item.filename)) : null,
          item.state === "ambiguous" ? button("Checked Slack — retry", () => act("POST", `/outbox/${item.id}/retry`, {},
            "Retry an unconfirmed post?", "Slack may have received the first post. Retry only after checking the original thread; this may create a duplicate.",
            item.text || item.filename, true), "danger") : null)))
      : empty("All posts are accounted for."))];
}

function select(value, options, change) {
  return el("select", { onchange: (event) => change(event.target.value) },
    options.map(([key, label]) => el("option", { value: key, ...(key === value ? { selected: "selected" } : {}) }, label)));
}
function searchBox() {
  return el("input", { class: "search", type: "search", placeholder: "Search, then press Enter", value: state.query,
    onkeydown: (event) => { if (event.key === "Enter") { state.query = event.target.value.trim(); render(); event.preventDefault(); } } });
}
function filteredThreads(threads, filter, query) {
  return threads.filter((item) => (filter === "all" || (filter === "active" ? item.control === "active" : item.control === filter)) &&
    (!query || [item.summary, item.context?.repo, item.context?.workspace, item.context?.branch, item.pause_reason]
      .some((value) => (value || "").toLowerCase().includes(query.toLowerCase()))));
}
function table(headers, rows) {
  return el("div", { class: "table-wrap" }, el("table", { class: "data-table" },
    el("thead", {}, el("tr", {}, headers.map((label) => el("th", {}, label)))), el("tbody", {}, rows)));
}
function threadsView(data) {
  const threads = filteredThreads(data.threads, state.threadFilter, state.query);
  const toolbar = el("div", { class: "toolbar" }, searchBox(), select(state.threadFilter,
    [["active", "Active"], ["paused", "Paused"], ["closed", "Closed"], ["archived", "Archived"], ["all", "All threads"]],
    (value) => { state.threadFilter = value; render(); }),
    el("div", { class: "segmented" },
      button("List", () => { state.threadLayout = "list"; render(); }, state.threadLayout === "list" ? "active" : ""),
      button("Cards", () => { state.threadLayout = "cards"; render(); }, state.threadLayout === "cards" ? "active" : "")));
  const cards = threads.map((item) => el("article", { class: "worker-card" },
    el("div", { class: "item-top" }, el("h3", {}, threadTitle(item).slice(0, 160)), statusPill(item.control === "active" ? item.status : item.control)),
    el("p", {}, threadContext(item) || "Slack conversation"),
    el("div", { class: "card-foot" }, el("span", { class: "small muted" }, `${item.turns} turns`), threadLink(item.id, "Open →"))));
  const list = table(["Conversation", "Context", "Status", "Turns", "Updated"], threads.map((item) => el("tr", {},
    el("td", {}, threadLink(item.id, threadTitle(item).slice(0, 110))),
    el("td", {}, [item.context?.repo || item.context?.workspace, item.context?.branch].filter(Boolean).join(" · ")),
    el("td", {}, statusPill(item.control === "active" ? item.status : item.control)),
    el("td", {}, item.turns), el("td", {}, ago(item.updated)))));
  return [page("Threads", "Slack threads and their jobs. Search covers loaded threads.",
    el("span", { class: "small muted" }, `${threads.length} shown`)), toolbar,
    threads.length ? (state.threadLayout === "cards" ? el("div", { class: "grid-cards" }, cards) : panel("Conversations", list))
      : empty("No conversations match these filters."),
    data.threads.length === state.threadLimit ? el("div", { class: "form-actions" },
      button("Load older conversations", () => { state.threadLimit += 200; return refresh(true); }, "button")) : null];
}

function pair(label, value) { return el("div", { class: "label-pair" }, el("dt", {}, label), el("dd", {}, value || "—")); }
function threadActions(session) {
  const path = `/threads/${encodeURIComponent(session.id)}`;
  const change = (action, title, description, dangerous = false) => button(title, () => act("POST", `${path}/${action}`, {},
    `${title} this thread?`, description, "", dangerous), dangerous ? "danger" : "");
  return el("div", { class: "actions" },
    session.control === "active" ? change("pause", "Pause", "New work in this thread will stop until you resume it.") : null,
    session.control === "paused" ? change("resume", "Resume", "The thread will resume and may reply to the last unanswered message.") : null,
    session.control !== "closed" && session.control !== "archived" && session.control !== "cleaned" ? change("close", "Close", "This closes the thread and stops its workers.", true) : null,
    session.control === "closed" ? change("archive", "Archive", "This moves the closed thread out of the active view.") : null,
    session.control === "archived" ? change("restore", "Restore", "This returns the thread to the list.") : null,
    session.control === "closed" || session.control === "archived" ? change("clean", "Clear thread content", "This removes locally stored message text, owner instructions, summary, and decisions. This cannot be undone.", true) : null,
    el("a", { class: "button", href: slackUrl(session), target: "_blank", rel: "noopener noreferrer" }, "Open in Slack ↗"));
}
function instructionPanel(session, instructions) {
  const disabled = state.data.status.observe_only || ["closed", "archived", "cleaned"].includes(session.control);
  const draft = state.instructionDraft?.thread === session.id ? state.instructionDraft.text : "";
  const form = disabled ? el("p", { class: "sub" }, state.data.status.observe_only
    ? "Instructions are unavailable in observe-only mode." : "Restore this thread before giving it a new instruction.")
    : el("div", {},
      el("p", { class: "small sub" }, "Give this thread a private instruction. Work still follows its access and approval rules; replies may appear in Slack."),
      el("textarea", { id: "owner-instruction", rows: "4", maxlength: "4000", placeholder: "What should Fridica do next?",
        oninput: (event) => { state.instructionDraft = { thread: session.id, text: event.target.value }; } }, draft),
      el("div", { class: "form-actions" }, button("Send instruction", async () => {
        const text = document.getElementById("owner-instruction").value.trim();
        if (!text) { toast("Enter an instruction first.", true); return; }
        const current = state.instructionDraft;
        const clientId = current?.thread === session.id && current.text.trim() === text && current.id
          ? current.id : crypto.randomUUID();
        state.instructionDraft = { thread: session.id, text, id: clientId };
        if (!await confirmAction("Send instruction?",
          "Fridica may delegate work or reply in Slack. Access and approval rules still apply.", text)) return;
        await api("POST", `/threads/${encodeURIComponent(session.id)}/instruct`, { text, client_id: clientId });
        state.instructionDraft = null;
        toast("Instruction queued for this conversation.");
        await refresh(true);
      }, "primary")));
  return [form, instructions.length ? el("div", { class: "instruction-history" },
    el("div", { class: "eyebrow" }, "RECENT INSTRUCTIONS"), instructions.map((item) =>
      el("div", { class: "instruction" },
        el("div", { class: "item-top" }, el("span", { class: "small muted" }, when(item.created)),
          statusPill(item.state === "done" ? "processed" : item.state)),
        el("p", {}, item.text)))) : null];
}
function threadDetail(detail) {
  const session = detail.session;
  const members = new Map();
  for (const message of detail.messages) if (message.source !== "self" && !members.has(message.sender))
    members.set(message.sender, `Member ${members.size + 1}`);
  return [el("div", { class: "detail-head" }, button("← Threads", () => go("threads"), "button"), statusPill(session.control === "active" ? session.status : session.control)),
    page(threadTitle(session), threadContext(session) || "Slack conversation"),
    el("div", { class: "detail-grid" },
      el("div", { class: "stack" },
        panel("Summary", [el("p", { class: "sub" }, session.summary || "No summary yet."),
          session.decisions?.length ? el("div", {}, el("h3", {}, "Decisions"), el("ul", {}, session.decisions.map((decision) => el("li", {}, decision)))) : null]),
        panel(`Messages · ${detail.messages.length}`, detail.messages.length ? detail.messages.map((message) =>
          el("div", { class: "message" + (message.source === "self" ? " self" : "") },
            el("div", { class: "small" }, message.source === "self" ? "Fridica" : members.get(message.sender), " · ", when(Number(message.ts))), message.text || "[No text]")) : empty("No message text is stored.")),
        detail.jobs.length ? panel("Jobs", table(["Work", "Status", "Result"], detail.jobs.map((job) =>
          el("tr", {}, el("td", {}, job.brief), el("td", {}, statusPill(job.status)),
            el("td", {}, job.result?.summary || job.error || "—"))))) : null),
      el("div", { class: "stack" },
        panel("Guide this thread", instructionPanel(session, (detail.instructions || []).filter((item) => item.text))),
        panel("Controls", threadActions(session)),
        panel("Context", el("dl", {}, pair("Repository", session.context?.repo), pair("Branch", session.context?.branch),
          pair("Machine", session.context?.machine), pair("Workspace", session.context?.workspace), pair("Backend", session.context?.backend),
          pair("Turns", session.turns), pair("Updated", when(session.updated)))),
        panel("Workers", detail.workers.length ? detail.workers.map((worker) => el("div", { class: "list-row" },
          el("div", {}, el("strong", {}, `${worker.role} · ${worker.machine}`), el("p", {}, worker.summary || worker.workspace)),
          statusPill(worker.status))) : empty("No workers in this conversation."))))];
}

function workersView(data) {
  const workers = data.workers.filter((worker) =>
    (state.workerFilter === "all" || (state.workerFilter === "current" ? worker.status !== "stopped" : worker.status === state.workerFilter)) &&
    (!state.query || [worker.machine, worker.workspace, worker.role, worker.summary, worker.backend]
      .some((value) => (value || "").toLowerCase().includes(state.query.toLowerCase()))));
  const cards = workers.map((worker) => {
    const job = data.jobs.find((item) => item.worker_id === worker.id);
    return el("article", { class: "worker-card" },
      el("div", { class: "item-top" }, el("h3", {}, `${worker.role} on ${worker.machine}`), statusPill(worker.status)),
      el("p", {}, worker.summary || job?.brief || "No current summary"),
      el("div", { class: "item-meta" }, worker.workspace, worker.backend, worker.process, job ? jobAge(job) : null),
      el("div", { class: "card-foot" }, threadLink(worker.session_id, "Open conversation →"),
        worker.status === "stopped" ? null : el("div", { class: "actions" },
          button("Interrupt", () => act("POST", `/workers/${encodeURIComponent(worker.id)}/interrupt`, {}, "Interrupt this worker?",
            "The current job will be interrupted; the worker can take another job later.")),
          button("Stop", () => act("POST", `/workers/${encodeURIComponent(worker.id)}/stop`, {}, "Stop this worker?",
            "The worker process will stop.", "", true), "danger"))));
  });
  return [page("Workers", "See where work is running and which conversation owns it."),
    el("div", { class: "toolbar" }, searchBox(), select(state.workerFilter,
      [["current", "Current"], ["running", "Running"], ["queued", "Queued"], ["awaiting_approval", "Awaiting approval"], ["all", "All workers"]],
      (value) => { state.workerFilter = value; render(); })),
    workers.length ? el("div", { class: "grid-cards" }, cards) : empty("No workers match these filters.")];
}

function networkSummary(machine, workspace) {
  const domains = workspace.network || [];
  if (!domains.length) return "Network off";
  if ((machine.backends || []).includes("codex")) return "Codex: full network; Claude: " + domains.join(", ");
  return "Network: " + domains.join(", ");
}

function machinesView(data) {
  return [page("Machines & access", "Where workers can run, what they can access, and current capacity."),
    el("div", { class: "grid-cards" }, data.machines.map((machine) => el("article", { class: "machine-card" },
      el("div", { class: "item-top" }, el("h3", {}, machine.name), statusPill(machine.transport)),
      el("p", {}, machine.transport === "slurm" ? "Slurm is configured; worker launch is not available yet."
        : machine.description || (machine.host ? `Host: ${machine.host}` : "Local machine")),
      el("div", { class: "item-meta" }, `${machine.busy_jobs}/${machine.max_jobs} jobs`, `${machine.live_workers}/${machine.max_workers} processes`),
      el("div", { class: "workspace-list" }, (machine.workspace_details || []).map((workspace) =>
        el("div", { class: "workspace" },
          el("div", { class: "item-top" }, el("strong", {}, workspace.name), statusPill(workspace.mode)),
          el("div", { class: "mono sub" }, workspace.path),
          el("div", { class: "small sub" }, `Approval: ${workspace.approvals} · ${networkSummary(machine, workspace)}`)))),
      el("div", { class: "card-foot" }, el("span", { class: "small muted" }, `Backends: ${(machine.backends || []).join(" · ")}`))))),
    panel("Access boundary", el("p", { class: "sub" }, "These are the effective workspace policies. Change folder access and policy in the local Fridica config; changes apply to new workers."))];
}

function activityView(data) {
  const entries = data.activity.filter((entry) => !state.query || [entry.actor, entry.action, entry.target]
    .some((value) => (value || "").toLowerCase().includes(state.query.toLowerCase())));
  return [page("Activity", "Recent control and worker events from the local audit log."), searchBox(),
    panel(`Events · ${entries.length}`, entries.length ? entries.map((entry) => el("div", { class: "event" },
      el("strong", {}, entry.action), el("p", {}, [entry.actor, entry.target].filter(Boolean).join(" · ")),
      el("span", { class: "tiny muted" }, when(entry.time)))) : empty("No events match your search."))];
}

function field(label, id, value, help = "", type = "text") {
  return el("div", { class: "field" }, el("label", { for: id }, label),
    el("input", { id, type, value: String(value ?? "") }), help ? el("small", {}, help) : null);
}
function settingsView(data) {
  const config = data.config, parent = config.parent, limits = config.limits;
  const parentKeys = ["backend", "model", "triage_model", "reasoning_effort"];
  const limitKeys = ["max_wait_replies", "max_no_progress", "max_delegations_per_turn", "max_workers_per_thread", "max_jobs", "job_timeout", "worker_idle"];
  return [page("Settings", "Current behavior for this local Fridica instance."),
    panel("Agent model", [el("p", { class: "small muted" }, "Blank model fields use the backend default."),
      el("div", { class: "form-grid" },
        el("div", { class: "field" }, el("label", { for: "backend" }, "Backend"),
          el("select", { id: "backend" }, ["claude", "codex"].map((value) =>
            el("option", { value, ...(value === parent.backend ? { selected: "selected" } : {}) }, value)))),
        field("Model", "model", parent.model, "Backend model name"),
        field("Triage model", "triage_model", parent.triage_model),
        el("div", { class: "field" }, el("label", { for: "reasoning_effort" }, "Reasoning effort"),
          el("select", { id: "reasoning_effort" }, ["", "low", "medium", "high", "xhigh", "max"].map((value) =>
            el("option", { value, ...(value === parent.reasoning_effort ? { selected: "selected" } : {}) }, value || "Backend default"))))),
      el("div", { class: "form-actions" }, button("Save model settings", async () => {
        const changes = Object.fromEntries(parentKeys.map((key) => [key, document.getElementById(key).value.trim()]));
        await act("PATCH", "/config/parent", changes, "Save model settings?",
          "New settings apply when Fridica picks up the updated configuration.");
      }, "primary"))]),
    panel("Loop and workload limits", [el("div", { class: "form-grid" }, limitKeys.map((key) =>
      field(key.replaceAll("_", " "), key, limits[key], key.includes("timeout") || key === "worker_idle" ? "Seconds" : "", "number"))),
    el("div", { class: "form-actions" }, button("Save limits", async () => {
        const changes = Object.fromEntries(limitKeys.map((key) => [key, Number(document.getElementById(key).value)]));
        await act("PATCH", "/config/limits", changes, "Save workload limits?",
          "These values govern reply loops and concurrent work. Invalid values will be rejected.");
      }, "primary"))])];
}

function showUnlock() {
  state.data = {};
  document.getElementById("view").replaceChildren();
  document.getElementById("unlock").hidden = false;
  document.getElementById("status").textContent = "Locked";
  document.getElementById("status").className = "connection bad";
}
function render() {
  const data = state.data;
  if (!data.status) return;
  const content = state.view === "threads" && state.thread ? (data.detail ? threadDetail(data.detail) : [empty("Loading conversation…")])
    : ({ overview, inbox, threads: threadsView, workers: workersView, machines: machinesView,
      activity: activityView, settings: settingsView })[state.view](data);
  document.getElementById("view").replaceChildren(...content.flat().filter(Boolean));
}
async function refresh(force = false) {
  if (state.refreshing) { if (force) state.pending = true; return; }
  if (!force && (document.hidden || !state.auto || document.activeElement?.matches("input, select, textarea") ||
    document.getElementById("confirm-dialog").open)) return;
  state.refreshing = true;
  try {
    const [status, approvals, outbox, threads, jobs, workers, attentionThreads] = await Promise.all([
      api("GET", "/status"), api("GET", "/approvals"), api("GET", "/outbox"),
      api("GET", `/threads?limit=${state.threadLimit}`), api("GET", "/jobs"), api("GET", "/workers"),
      api("GET", "/attention/threads")]);
    const view = state.view, thread = state.thread;
    const extra = view === "threads" && thread ? { detail: await api("GET", "/threads/" + encodeURIComponent(thread)) }
      : view === "machines" ? { machines: await api("GET", "/machines") }
      : view === "activity" ? { activity: await api("GET", "/activity") }
      : view === "settings" ? { config: await api("GET", "/config") } : {};
    if (view !== state.view || thread !== state.thread) { state.pending = true; return; }
    state.data = { status, approvals, outbox, threads, jobs, workers, attentionThreads, ...extra };
    document.getElementById("status").textContent = status.slack === "connected" ? "Connected" : status.slack;
    document.getElementById("status").className = "connection " + (status.slack === "connected" ? "ok" : "bad");
    document.getElementById("inbox-count").textContent = attention(state.data).length || "";
    document.getElementById("last-sync").textContent = "Updated just now";
    document.getElementById("unlock").hidden = true;
    render();
  } catch (error) {
    if (error.message !== "locked") {
      document.getElementById("status").textContent = "Offline";
      document.getElementById("status").className = "connection bad";
      document.getElementById("view").replaceChildren(page("Connection lost", "Fridica is not responding."),
        empty(error.message), button("Retry", () => refresh(true), "button"));
    }
  } finally {
    state.refreshing = false;
    if (state.pending) { state.pending = false; refresh(true); }
  }
}

function start() {
  state.key = readKey();
  if (!state.key) showUnlock();
  document.getElementById("unlock-form").addEventListener("submit", (event) => {
    event.preventDefault();
    state.key = document.getElementById("key").value.trim();
    try { sessionStorage.setItem("fridica-key", state.key); } catch (_) { /* storage may be blocked */ }
    document.getElementById("unlock").hidden = true;
    refresh(true);
  });
  for (const tab of document.querySelectorAll("[data-view]")) tab.addEventListener("click", () => go(tab.dataset.view));
  document.getElementById("refresh-button").addEventListener("click", () => refresh(true));
  document.getElementById("auto-button").addEventListener("click", (event) => {
    state.auto = !state.auto;
    event.currentTarget.textContent = state.auto ? "Live on" : "Live off";
    event.currentTarget.setAttribute("aria-pressed", String(state.auto));
    if (state.auto) refresh(true);
  });
  if (state.key) refresh(true);
  setInterval(() => refresh(), 10000);
}

if (typeof document !== "undefined") start();
if (typeof module !== "undefined") module.exports = { el, ago, statusPill, filteredThreads, attention, networkSummary };
