"use strict";
const graph = typeof module !== "undefined" ? require("./graph.js") : FridicaGraph;

const state = {
  view: "overview", thread: null, key: "", auto: true, data: {},
  query: "", workFilter: "current", threadFilter: "active", threadLayout: "list", threadLimit: 200,
  refreshing: false, pending: false, instructionDraft: null, lastSync: null,
  selectedParent: null, historyOpen: false, descriptionOpen: false,
  graphMode: "workers", graphViewport: null, workDetail: null, workLoading: null, workAttempted: null,
  workRequest: 0, workError: "", workLimit: 200, replayIndex: null, replayPlaying: false,
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
  for (const child of children.flat(Infinity)) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function inlineMarkdown(text) {
  const parts = [];
  const tokens = /\[([^\]]+)\]\((https?:\/\/[^\s)]+)\)|`([^`]+)`|\*\*([^*\n]+)\*\*|\*([^*\n]+)\*/g;
  let start = 0;
  for (const match of text.matchAll(tokens)) {
    parts.push(text.slice(start, match.index));
    if (match[1]) parts.push(el("a", { href: match[2], target: "_blank", rel: "noopener noreferrer" }, match[1]));
    else if (match[3]) parts.push(el("code", {}, match[3]));
    else if (match[4]) parts.push(el("strong", {}, match[4]));
    else parts.push(el("em", {}, match[5]));
    start = match.index + match[0].length;
  }
  parts.push(text.slice(start));
  return parts;
}

function slackText(text) {
  return String(text || "").replace(/&(amp|lt|gt);/g, (_, name) => ({ amp: "&", lt: "<", gt: ">" })[name]);
}
function markdown(text) {
  const lines = slackText(text).replace(/\r\n?/g, "\n").split("\n");
  const blocks = [];
  const listLine = (line) => line.match(/^(?:([-*])|(\d+)\.)\s+(.+)$/);
  for (let i = 0; i < lines.length;) {
    if (!lines[i].trim()) { i++; continue; }
    if (lines[i].startsWith("```")) {
      const code = [];
      for (i++; i < lines.length && !lines[i].startsWith("```"); i++) code.push(lines[i]);
      blocks.push(el("pre", {}, el("code", {}, code.join("\n"))));
      if (i < lines.length) i++;
      continue;
    }
    const heading = lines[i].match(/^(#{1,3})\s+(.+)$/);
    if (heading) {
      blocks.push(el("h" + (heading[1].length + 1), {}, inlineMarkdown(heading[2])));
      i++;
      continue;
    }
    const list = listLine(lines[i]);
    if (list) {
      const ordered = Boolean(list[2]);
      const items = [];
      let match;
      while (i < lines.length && (match = listLine(lines[i])) && Boolean(match[2]) === ordered) {
        items.push(el("li", {}, inlineMarkdown(match[3])));
        i++;
      }
      blocks.push(el(ordered ? "ol" : "ul", {}, items));
      continue;
    }
    const paragraph = [];
    while (i < lines.length && lines[i].trim() && !lines[i].startsWith("```") &&
      !/^(#{1,3})\s+.+$/.test(lines[i]) && !listLine(lines[i])) paragraph.push(lines[i++]);
    blocks.push(el("p", {}, inlineMarkdown(paragraph.join("\n"))));
  }
  return el("div", { class: "rich-text" }, blocks);
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
    const target = event.currentTarget;
    target.disabled = true;
    try { await handler(); } catch (error) { toast(error.message, true); }
    finally { target.disabled = false; }
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
  dialog.returnValue = "cancel";
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
  state.replayPlaying = false;
  state.view = view;
  state.thread = thread;
  state.query = "";
  document.getElementById("current-view").textContent = ({ inbox: "Needs you", threads: "Conversations",
    machines: "Machines & access", activity: "Activity", settings: "Settings" })[view] || "Work";
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
function threadTitle(thread) {
  return thread.summary || [thread.context?.repo, thread.context?.workspace].filter(Boolean).join(" · ") || "Slack conversation";
}
function taskTitle(value) {
  const words = String(value || "").replace(/^(?:Home thread|THREAD)\s+\S+\s*/i, "")
    .split("\n")[0].split(/\s+/).slice(0, 7);
  while (words.length > 1 && words.join(" ").length > 44) words.pop();
  return words.join(" ") || "Worker task";
}
function workTitle(thread) {
  const summary = threadTitle(thread).replace(/^(?:Home thread|THREAD)\s+\S+\s*/i, "").replace(/\s+/g, " ").trim();
  const repos = String(thread.context?.repo || thread.context?.workspace || "").split(/[;,]/).map(value => value.trim()).filter(Boolean);
  const repo = repos[0] ? repos[0] + (repos.length > 1 ? " +" + (repos.length - 1) + " repos" : "") : "";
  const refs = [...new Set(summary.match(/#\d+/g) || [])];
  if (repo && refs.length) return `${repo} · ${refs.slice(0, 2).join(", ")}${refs.length > 2 ? ` +${refs.length - 2}` : ""}`;
  return summary.split(/[.!?;:]/)[0].split(" ").slice(0, 7).join(" ") || repo || "Slack conversation";
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

function latestJobs(jobs) {
  const latest = new Map();
  for (const job of jobs) {
    if (graph.preferJob(job, latest.get(job.worker_id))) latest.set(job.worker_id, job);
  }
  return latest;
}
function workerState(worker, job) {
  if (worker.status === "awaiting_approval") return "blocked";
  const status = graph.state(job?.status || worker.status);
  if (status === "blocked" && job?.finished_at && ["idle", "stopped"].includes(worker.status)) return "done";
  return status;
}
function workerPlace(worker, machines) {
  const machine = (machines || []).find((item) => item.name === worker.machine);
  const workspace = machine?.workspace_details?.find((item) => item.name === worker.workspace);
  return { host: worker.machine || "Unknown host", mode: workspace?.mode || "", transport: machine?.transport || "", environment: [machine?.transport?.toUpperCase() ||
    (worker.machine === "local" ? "LOCAL" : ""), worker.workspace, workspace?.mode].filter(Boolean).join(" · ") || "Environment not recorded" };
}
function workItems(thread, data, latest) {
  return (data.workers || []).filter((worker) => worker.session_id === thread.id).map((worker) => {
    const job = latest.get(worker.id);
    return { worker, job, status: workerState(worker, job), place: workerPlace(worker, data.machines) };
  });
}
function workFlags(thread, items) {
  const closed = ["closed", "archived", "cleaned"].includes(thread.control);
  const live = items.some((item) => ["running", "queued"].includes(item.status) ||
    item.worker?.status === "awaiting_approval");
  const completed = closed || (thread.control === "active" && thread.status === "complete" && !live);
  const running = !completed && items.some((item) => item.status === "running");
  const queued = !completed && items.some((item) => item.status === "queued");
  const blocked = !completed && (thread.control === "paused" || items.some((item) => item.status === "blocked") ||
    (!running && !queued && thread.status === "blocked"));
  const waiting = !completed && thread.status === "waiting" && !running && !queued && !blocked;
  return { current: running || queued || blocked || waiting || (!completed && thread.control === "active" && thread.status === "working"), running, queued, blocked, waiting, completed };
}
function workCounts(items) {
  const counts = { running: 0, queued: 0, blocked: 0, done: 0, idle: 0 };
  for (const item of items) counts[item.status]++;
  return counts;
}
function parentStatus(thread, flags) {
  if (thread.control !== "active") return thread.control;
  return flags.blocked ? "blocked" : flags.running ? "running" : flags.queued ? "queued" : thread.status;
}
function parentCard(thread, items, flags, selected) {
  const counts = workCounts(items);
  const hosts = [...new Set(items.map((item) => item.place.host))];
  return el("button", { class: "parent-card" + (selected ? " selected" : ""), "aria-pressed": String(selected),
    title: threadTitle(thread), onclick: () => chooseParent(thread.id) },
    el("div", { class: "parent-card-top" }, el("span", { class: "eyebrow" }, "PARENT AGENT"),
      statusPill(parentStatus(thread, flags))),
    el("strong", { class: "parent-name" }, workTitle(thread)),
    el("div", { class: "parent-context" }, [thread.context?.repo || thread.context?.workspace, ago(thread.updated)].filter(Boolean).join(" · ") || "Conversation"),
    el("div", { class: "parent-graph" },
      el("span", { class: "work-ring" },
        el("strong", {}, items.length)),
      el("span", { class: "graph-key" },
        ["running", "queued", "blocked", "done"].map((status) =>
          el("span", {}, el("i", { class: "state-dot " + status }), status === "done" ? "finished" : status, el("b", {}, counts[status]))))),
    el("div", { class: "parent-card-foot" }, hosts.length ? hosts.slice(0, 2).join(" · ") + (hosts.length > 2 ? ` +${hosts.length - 2}` : "") : "No worker host",
      el("span", {}, "View →")));
}
function workerLabel(worker) {
  return (worker.role || "Worker") + (worker.id ? " · " + worker.id.slice(-4) : "");
}
function workerDetail(thread, item, readOnly = false) {
  const { worker, job, status, place } = item;
  return [el("div", { class: "eyebrow" }, "WORKER DETAIL"),
    el("h2", {}, taskTitle(job?.brief || worker.summary || worker.role || "Worker")),
    el("p", { class: "sub" }, workTitle(thread)),
    el("div", { class: "worker-facts" },
      [["Status", !readOnly && worker.status === "awaiting_approval" ? "awaiting approval" : job?.status || worker.status || "idle"], ["Role", worker.role || "Worker"],
        ["Server", place.host], ["Environment", place.environment], ["Backend", worker.backend || "—"], ["Runtime", job ? duration(graph.elapsed(job, item.observedAt)) || "—" : "—"],
        ["Started", job?.started_at ? when(job.started_at) : "—"], ["Finished", job?.finished_at ? when(job.finished_at) : "—"]]
        .map(([label, value]) => el("div", {}, el("small", {}, label), el("strong", {}, value)))),
    status === "blocked" || job?.status === "failed" || job?.error || (!readOnly && thread.pause_reason) ?
      el("section", { class: "detail-note alert" }, el("h3", {}, !readOnly && worker.status === "awaiting_approval" ? "Approval pending" : "Why it stopped"),
        markdown(!readOnly && worker.status === "awaiting_approval" ? "Review the request in Needs you." :
          job?.error || (!readOnly && thread.pause_reason) || "No reason recorded.")) : null,
    job?.result?.summary ? el("section", { class: "detail-note" }, el("h3", {}, "Result"), markdown(job.result.summary)) : null,
    job?.brief ? el("section", { class: "detail-note" }, el("h3", {}, "Task"), markdown(job.brief)) : null,
    !readOnly && worker.status !== "stopped" ? el("div", { class: "actions detail-note" },
      ["running", "awaiting_approval"].includes(worker.status) ? button("Interrupt", () => {
        document.getElementById("worker-dialog").close();
        return act("POST", `/workers/${encodeURIComponent(worker.id)}/interrupt`, {}, "Interrupt this worker?",
          "The current job will stop; the worker can take another job later.");
      }) : null,
      button("Stop", () => {
        document.getElementById("worker-dialog").close();
        return act("POST", `/workers/${encodeURIComponent(worker.id)}/stop`, {}, "Stop this worker?",
          "The worker process will stop.", "", true);
      }, "danger")) : null,
    el("div", { class: "detail-note" }, threadLink(thread.id, "Open conversation →"))];
}
function openWorker(thread, item, readOnly = false) {
  try {
    document.getElementById("worker-detail").replaceChildren(...workerDetail(thread, item, readOnly).filter(Boolean));
    const dialog = document.getElementById("worker-dialog");
    dialog.showModal();
    dialog.scrollTop = 0;
  } catch (error) { toast(`Could not show worker detail: ${error.message}`, true); }
}
function workerRow(thread, item, readOnly = false) {
  const { worker, job, status, place } = item;
  const age = readOnly && item.observedAt ? "Elapsed " + (duration(graph.elapsed(job, item.observedAt)) || "—") :
    status === "running" || status === "queued" ? jobAge(job) :
    job?.finished_at ? `Finished ${ago(job.finished_at)}` : worker.updated ? `Last active ${ago(worker.updated)}` : "";
  return el("button", { class: "work-worker", onclick: () => openWorker(thread, item, readOnly) },
    el("span", { class: "worker-glyph " + status }, (worker.role || "W").slice(0, 1).toUpperCase()),
    el("span", { class: "worker-copy" },
      el("strong", {}, taskTitle(job?.brief || worker.summary || "No recent job")),
      el("small", {}, [workerLabel(worker), age].filter(Boolean).join(" · "))),
    el("span", { class: "worker-location" }, el("small", {}, "SERVER / ENVIRONMENT"),
      el("strong", {}, place.host), el("em", {}, place.environment)),
    el("span", { class: "worker-status " + status }, job?.status === "failed" ? "Failed" : status === "done" ? "Finished" : status),
    el("span", { class: "worker-chevron" }, "›"));
}

function chooseParent(id) {
  document.querySelector(".work-main")?.scrollTo(0, 0);
  state.selectedParent = id;
  state.workDetail = null; state.workAttempted = null; state.workError = "";
  state.workLimit = 200; state.replayIndex = null; state.replayPlaying = false;
  state.graphViewport = null; state.descriptionOpen = false;
  state.historyOpen = state.workFilter === "completed";
  render();
}
async function loadWorkDetail(force = false) {
  const id = state.selectedParent, limit = state.workLimit, key = id + ":" + limit;
  if (!id || state.view !== "overview" || state.workLoading === key ||
    (!force && state.workAttempted === key)) return;
  const request = ++state.workRequest;
  state.workLoading = key; state.workAttempted = key; state.workError = "";
  try {
    const detail = await api("GET", "/threads/" + encodeURIComponent(id) + "?limit=" + limit);
    if (state.selectedParent !== id || request !== state.workRequest) return;
    if (state.replayIndex !== null) {
      const selected = graph.events(state.workDetail || {})[state.replayIndex];
      const index = graph.events(detail).findIndex(event => event.id === selected?.id);
      if (index < 0) { state.workError = "History changed. Return to live to refresh."; return; }
      state.replayIndex = index;
    }
    state.workDetail = detail;
  } catch (error) {
    if (state.selectedParent === id && request === state.workRequest) state.workError = error.message;
  } finally {
    if (request === state.workRequest) {
      state.workLoading = null;
      if (state.view === "overview") render();
    }
  }
}
function duration(seconds) {
  if (seconds === null) return "";
  const value = Math.max(0, Math.floor(seconds));
  return value < 60 ? value + "s" : value < 3600 ? Math.floor(value / 60) + "m " + value % 60 + "s" :
    Math.floor(value / 3600) + "h " + Math.floor(value % 3600 / 60) + "m";
}
function blockReason(thread, item, replay = false) {
  return !replay && item.worker.status === "awaiting_approval" ? "Waiting for owner approval" :
    item.job?.error === "worker_isolation_runtime_or_transport_failed" ?
      "Worker could not start: isolation runtime or transport failed." :
      item.job?.error || (!replay && thread.pause_reason) || "No reason was recorded.";
}
function aliasKey(thread) { return "fridica-names:" + state.data.status.owner + ":" + (thread.key?.workspace || ""); }
function graphAliases(thread) {
  try { return JSON.parse(localStorage.getItem(aliasKey(thread)) || "{}"); } catch (_) { return {}; }
}
function openParticipant(thread, node, messages) {
  const aliases = graphAliases(thread);
  document.getElementById("worker-detail").replaceChildren(
    el("div", { class: "eyebrow" }, "PARTICIPANT"), el("h2", {}, node.label),
    el("p", { class: "sub" }, node.id),
    field("Display name", "participant-name", aliases[node.id] || node.label),
    button("Save display name", () => {
      const value = document.getElementById("participant-name").value.trim().slice(0, 64);
      if (value) aliases[node.id] = value; else delete aliases[node.id];
      try { localStorage.setItem(aliasKey(thread), JSON.stringify(aliases)); }
      catch (_) { toast("Display names cannot be saved in this browser.", true); return; }
      document.getElementById("worker-dialog").close(); render();
    }, "primary"),
    el("div", { class: "participant-messages" }, messages.filter(m => Number(m.ts) > 0 &&
      (m.sender || (m.source === "self" ? state.data.status.owner : "unknown")) === node.id)
      .sort((a, b) => Number(b.ts) - Number(a.ts)).slice(0, 5).map(m => el("article", { class: "message" }, el("small", {}, when(Number(m.ts))), markdown(m.text)))));
  document.getElementById("worker-dialog").showModal();
}
function openGuide(thread) {
  if (state.replayIndex !== null) return;
  const detail = state.workDetail?.session.id === thread.id ? state.workDetail : null;
  document.getElementById("worker-detail").replaceChildren(
    el("div", { class: "eyebrow" }, "GUIDE PARENT"), el("h2", {}, workTitle(thread)),
    ...instructionPanel(thread, detail?.instructions || []).filter(Boolean),
    el("div", { class: "detail-note" }, threadActions(thread)));
  document.getElementById("worker-dialog").showModal();
}
function graphHistory(thread, detail, events) {
  const aliases = graphAliases(thread);
  const selected = state.replayIndex === null ? events.length - 1 : Math.min(state.replayIndex, events.length - 1);
  const event = events[selected];
  const jump = index => { state.replayIndex = Math.max(0, Math.min(index, events.length - 1)); render(); };
  return el("section", { class: "orbit-history", "aria-label": "Recorded history" },
    el("div", { class: "history-toolbar" },
      el("div", {}, el("h2", {}, "History"), el("small", {}, events.length + " recorded events · idle time compressed")),
      el("div", { class: "actions" },
        button(state.replayPlaying ? "Pause replay" : "Play replay", () => {
          if (!events.length) return;
          if (state.replayIndex === null || state.replayIndex >= events.length - 1) state.replayIndex = 0;
          state.replayPlaying = !state.replayPlaying; render();
        }), button("Back to live", () => { state.replayPlaying = false; state.replayIndex = null; state.workError = ""; state.workAttempted = null; render(); }, "primary"))),
    events.length ? [
      el("input", { class: "replay-range", type: "range", min: "0", max: String(events.length - 1), value: String(selected),
        "aria-label": "Replay position", onchange: event => jump(Number(event.target.value)) }),
      el("div", { class: "replay-time" }, el("span", {}, when(events[0].time)), el("strong", {}, when(event.time)),
        el("span", {}, when(events[events.length - 1].time))),
      el("div", { class: "history-events" }, events.slice(Math.max(0, selected - 3), selected + 1).map(item =>
        el("button", { class: "history-event" + (item.id === event.id ? " active" : ""), onclick: () => jump(events.indexOf(item)) },
          el("small", {}, item.message ? (aliases[item.message.sender] || item.message.sender_name || item.message.sender || "Message") : item.kind),
          el("strong", {}, slackText(item.text).split("\n")[0].slice(0, 130)), el("time", {}, when(item.time))))),
      el("details", { class: "history-message" }, el("summary", {}, "Read full event"), markdown(event.text),
        event.message ? el("a", { href: "https://app.slack.com/archives/" + encodeURIComponent(thread.key?.channel || "") +
          "/p" + event.message.ts.replace(".", ""), target: "_blank", rel: "noopener noreferrer" }, "Open message in Slack ↗") : null)
    ] : empty("No message or job history is stored."),
    state.workLimit < 1000 && (detail.messages || []).length >= state.workLimit ? button("Load older messages", () => {
      state.workLimit += 200; return loadWorkDetail(true);
    }, "section-link") : null);
}
function workSnapshot(thread, items, data) {
  const detail = state.workDetail?.session.id === thread.id ? state.workDetail : null;
  const events = detail ? graph.events(detail) : [];
  const replay = state.replayIndex !== null && events.length > 0;
  const history = replay ? events.slice(0, state.replayIndex + 1) : [];
  const at = replay ? history.at(-1).time : Infinity;
  const messages = replay ? history.filter(e => e.message).map(e => e.message) : detail?.messages || [];
  return { detail, events, replay, at, messages, items: replay ?
    graph.snapshot(detail.workers || [], history, at).map(item => ({ ...item, place: workerPlace(item.worker, data.machines) })) : items };
}
function workerAge({ job, status }, at, replay) {
  if (!job) return status;
  const label = status === "done" || job.status === "failed" ? job.status : status;
  const seconds = status === "queued" && job.queued_at ? at - job.queued_at : graph.elapsed(job, at);
  const time = duration(seconds);
  const finished = !replay && job.finished_at ? ago(job.finished_at) : "";
  return [label, time && (job.finished_at ? time + " run" : time), finished].filter(Boolean).join(" · ");
}

function agentMap(thread, view, flags, shown, owner) {
  const { messages, replay, at } = view;
  let nodes, edges;
  if (state.graphMode === "conversation") {
    const conversation = graph.conversation(messages, owner, graphAliases(thread));
    nodes = conversation.nodes.map(node => ({ ...node, kind: "person", status: "idle",
      active: conversation.current?.sender === node.id, subtitle: node.count + (node.count === 1 ? " message" : " messages"), meta: node.label === node.id ? "" : node.id }));
    edges = conversation.edges;
  } else {
    nodes = [{ id: thread.id, kind: "parent", label: "Parent", subtitle: workTitle(thread),
      meta: replay ? "Historical view" : parentStatus(thread, flags), status: replay ? "idle" : parentStatus(thread, flags) },
      ...shown.map(item => ({ id: item.worker.id, kind: "worker", item, label: workerLabel(item.worker),
        subtitle: taskTitle(item.job?.brief || item.worker.summary || "No active job"),
        meta: [item.place.host, [item.place.transport.toUpperCase(), item.place.mode].filter(Boolean).join(" / ")].filter(Boolean).join(" · "),
        age: workerAge(item, replay ? at : Date.now() / 1000, replay),
        status: item.status === "done" && item.job?.status === "failed" ? "failed" : item.status }))];
    edges = shown.map(item => ({ source: thread.id, target: item.worker.id, active: item.status === "running" }));
  }
  return nodes.length ? graph.draw({ nodes, edges, viewport: state.graphViewport,
    onViewport: value => { state.graphViewport = value; },
    select: node => node.kind === "worker" ? openWorker(thread, node.item, replay) :
      node.kind === "person" ? openParticipant(thread, node, messages) :
      (state.descriptionOpen = !state.descriptionOpen, render()) }) : empty("No participants are recorded in the loaded messages.");
}
function workGraph(thread, view, flags, owner) {
  const { detail, events, replay, at, items } = view;
  const shown = items.filter(item => replay || state.historyOpen || !["done", "idle"].includes(item.status));
  const guide = button("Guide parent", () => openGuide(thread), "guide-parent");
  guide.disabled = replay;
  return el("section", { class: "work-orbit " + state.graphMode + "-view" },
    el("div", { class: "orbit-toolbar" },
      el("div", { class: "segmented", "aria-label": "Graph view" }, ["workers", "conversation", "list"].map(mode =>
        button({ workers: "Workers", conversation: "Conversation", list: "List" }[mode], () => {
          state.graphMode = mode; state.graphViewport = null; render();
        }, state.graphMode === mode ? "active" : ""))),
      el("span", { class: "orbit-source" }, replay ? "Replay · " + when(at) : "Live state"),
      replay ? null : button(state.historyOpen ? "Hide history" : "History", () => { state.historyOpen = !state.historyOpen; render(); }, "section-link"),
      guide),
    state.graphMode === "list" ? null : agentMap(thread, view, flags, shown, owner),
    state.graphMode === "workers" && shown.length ? el("div", { class: "orbit-roster" },
      shown.filter(item => !["done", "idle"].includes(item.status)).slice(0, 6).map(item => workerRow(thread, item, replay))) : null,
    state.graphMode === "workers" && !shown.length ? el("p", { class: "orbit-empty-note" }, "No active delegated worker. Open History to inspect earlier work.") : null,
    items.filter(item => item.status === "blocked").map(item => el("button", { class: "orbit-block",
      onclick: () => openWorker(thread, item, replay) }, el("strong", {}, item.worker.role || "Worker"),
      el("span", {}, blockReason(thread, item, replay)))),
    state.workError ? el("div", { class: "orbit-load-error" }, "History unavailable: " + state.workError,
      button("Retry history", () => loadWorkDetail(true))) : null,
    !detail && !state.workError ? el("p", { class: "orbit-loading" }, "Loading recorded history…") : null,
    state.graphMode === "conversation" ? el("div", { class: "orbit-foot" }, "Links show Slack @mentions.") : null,
    (state.historyOpen || replay) && detail ? graphHistory(thread, detail, events) : null);
}

function overview(data) {
  const latest = latestJobs(data.jobs || []);
  const all = (data.threads || []).map((thread) => {
    const workers = workItems(thread, data, latest);
    return { thread, workers, flags: workFlags(thread, workers) };
  });
  const query = state.query.toLowerCase();
  const priority = ({ workers, flags }) => Number(flags.blocked) * 3 + Number(flags.running) * 6 +
    Number(flags.queued) + (workers.some((item) => item.status === "blocked") ? 2 : Number(workers.length > 0));
  const visible = all.filter(({ thread, workers, flags }) =>
    (state.workFilter === "all" || flags[state.workFilter]) && (!query ||
      [threadTitle(thread), thread.context?.repo, thread.context?.branch, thread.context?.workspace, ...workers.map((item) => item.job?.brief)]
        .some((value) => (value || "").toLowerCase().includes(query))))
    .sort((a, b) => priority(b) - priority(a) || (b.thread.updated || 0) - (a.thread.updated || 0));
  if (!all.some(({ thread }) => thread.id === state.selectedParent)) {
    state.selectedParent = visible[0]?.thread.id || null;
    state.historyOpen = state.workFilter === "completed";
    state.descriptionOpen = false;
    state.replayIndex = null; state.replayPlaying = false; state.graphViewport = null;
  }
  const selectedEntry = all.find(({ thread }) => thread.id === state.selectedParent);
  const selected = selectedEntry?.thread;
  const selectedFlags = selectedEntry?.flags;
  const selectedItems = selectedEntry?.workers || [];
  const view = selected ? workSnapshot(selected, selectedItems, data) : { items: [] };
  const { replay, items: visibleItems } = view;
  const current = selectedFlags?.completed && !replay ? [] : visibleItems.filter(item => !["done", "idle"].includes(item.status));
  const past = selectedFlags?.completed && !replay ? visibleItems : visibleItems.filter(item => ["done", "idle"].includes(item.status));
  return [el("div", { class: "work-layout" },
    el("aside", { class: "parent-pane", "aria-label": "Parent agents" },
      el("div", { class: "eyebrow" }, "WORK GRAPH"), el("h1", {}, "Parents"),
      el("p", { class: "sub small" }, "Select a parent to see its workers."),
      el("div", { class: "parent-filter" }, searchBox(), select(state.workFilter,
        [["current", "Current"], ["running", "Running"], ["queued", "Queued"],
          ["blocked", "Blocked"], ["waiting", "Waiting"], ["completed", "Completed"], ["all", "All loaded"]]
          .map(([key, label]) => [key, `${label} (${all.filter((entry) => key === "all" || entry.flags[key]).length})`]),
        (value) => { state.workFilter = value; state.selectedParent = null; state.historyOpen = value === "completed"; render(); window.scrollTo(0, 0); }, "Filter parents")),
      selectedEntry && !visible.includes(selectedEntry) ? el("p", { class: "sub small" },
        "Selected parent no longer matches this filter.") : null,
      el("div", { class: "parent-cards" }, visible.length ? visible.map(({ thread, workers, flags }) =>
        parentCard(thread, workers, flags, thread.id === state.selectedParent)) :
        empty("No matching parent agents.")),
      state.threadLimit < 1000 && data.threads.length === state.threadLimit ? button("Load older", () => { state.threadLimit += 200; return refresh(true); }, "button") : null),
    el("div", { class: "work-main" }, selected ? [
      el("div", { class: "work-main-head" },
        el("div", {}, el("div", { class: "eyebrow" }, state.workFilter === "completed" ? "PARENT / WORK HISTORY" : "PARENT / CURRENT WORK"),
          el("h1", {}, workTitle(selected)),
          el("p", { class: "sub" }, threadContext(selected) || "Slack conversation"),
          el("div", { class: "parent-description" },
            el("button", { class: "section-link", onclick: () => { state.descriptionOpen = !state.descriptionOpen; render(); } },
              state.descriptionOpen ? "Hide full description −" : "Read full description +"),
            state.descriptionOpen ? markdown(threadTitle(selected)) : null)),
        el("span", { class: "head-count" }, el("strong", {}, visibleItems.filter(item => ["running", "queued"].includes(item.status)).length), " active", el("small", {}, visibleItems.length + (visibleItems.length === 1 ? " total worker" : " total workers")))),
      workGraph(selected, view, selectedFlags, data.status.owner),
      selectedFlags.blocked && state.replayIndex === null ? el("div", { class: "parent-alert" },
        el("strong", {}, selected.control === "paused" ? "Parent paused" : "Parent reports blocked"),
        el("span", {}, selected.pause_reason || selected.control_detail?.reason || "No reason is stored in the parent record."),
        threadLink(selected.id, "Review →")) : null,
      state.graphMode === "list" && (state.workFilter !== "completed" || current.length) ? [
        el("div", { class: "work-section-head" }, el("h2", {}, replay ? "Workers at this time" : "Current workers"),
          el("span", {}, `${current.length} shown`)),
        el("div", { class: "work-worker-list" }, current.length ? current.map((item) => workerRow(selected, item, replay)) :
          empty("No worker is running, queued, or blocked."))] : null,
      state.graphMode === "list" && !replay ? el("button", { class: "history-toggle", "aria-expanded": String(state.historyOpen),
        onclick: () => { state.historyOpen = !state.historyOpen; render(); } },
        `Completed & idle workers · ${past.length}`, state.historyOpen ? "−" : "+") : null,
      (state.historyOpen || replay) && state.graphMode === "list" ? el("div", { class: "work-worker-list" }, past.map((item) => workerRow(selected, item, replay))) : null,
      el("div", { class: "work-footer" }, threadLink(selected.id, "Open conversation →"))
    ] : empty("No parents match this filter. Try All loaded.")))];
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

function select(value, options, change, label = "") {
  return el("select", { onchange: (event) => change(event.target.value), ...(label ? { "aria-label": label } : {}) },
    options.map(([key, label]) => el("option", { value: key, ...(key === value ? { selected: "selected" } : {}) }, label)));
}
function searchBox() {
  return el("input", { class: "search", type: "search", placeholder: "Search, then press Enter", value: state.query,
    onkeydown: (event) => { if (event.key === "Enter") { state.query = event.target.value.trim(); state.selectedParent = null; render();
      if (state.view === "overview") window.scrollTo(0, 0); event.preventDefault(); } } });
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
  return [page("Conversations", "Slack threads and their jobs. Search covers loaded threads.",
    [el("span", { class: "small muted" }, `${threads.length} shown`), button("Activity log →", () => go("activity"), "section-link")]), toolbar,
    threads.length ? (state.threadLayout === "cards" ? el("div", { class: "grid-cards" }, cards) : panel("Conversations", list))
      : empty("No conversations match these filters."),
    state.threadLimit < 1000 && data.threads.length === state.threadLimit ? el("div", { class: "form-actions" },
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
        const input = document.getElementById("owner-instruction");
        const text = input.value.trim();
        if (!text) { toast("Enter an instruction first.", true); return; }
        const current = state.instructionDraft;
        const clientId = current?.thread === session.id && current.text.trim() === text && current.id
          ? current.id : crypto.randomUUID();
        state.instructionDraft = { thread: session.id, text, id: clientId };
        if (!await confirmAction("Send instruction?",
          "Fridica may delegate work or reply in Slack. Access and approval rules still apply.", text)) return;
        await api("POST", `/threads/${encodeURIComponent(session.id)}/instruct`, { text, client_id: clientId });
        if (state.instructionDraft?.id === clientId) {
          state.instructionDraft = null;
          input.value = "";
        }
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
  const aliases = graphAliases(session);
  return [el("div", { class: "detail-head" }, button("← Conversations", () => go("threads"), "button"), statusPill(session.control === "active" ? session.status : session.control)),
    page(threadTitle(session), threadContext(session) || "Slack conversation"),
    el("div", { class: "detail-grid" },
      el("div", { class: "stack" },
        panel("Summary", [markdown(session.summary || "No summary yet."),
          session.decisions?.length ? el("div", {}, el("h3", {}, "Decisions"), el("ul", {}, session.decisions.map((decision) => el("li", {}, decision)))) : null]),
        panel(`Messages · ${detail.messages.length}`, detail.messages.length ? detail.messages.map((message) =>
          el("div", { class: "message" + (message.source === "self" ? " self" : "") },
            el("div", { class: "small", title: message.sender }, aliases[message.sender] || message.sender_name || (message.source === "self" ? "Fridica" : message.sender), " · ", when(Number(message.ts))), markdown(message.text || "[No text]"))) : empty("No message text is stored.")),
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
  const limitKeys = ["max_wait_replies", "max_no_progress", "max_delegations_per_turn", "max_workers_per_thread", "max_jobs", "job_timeout", "worker_idle"].filter((key) => Object.hasOwn(limits, key));
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
    panel("Workload limits", [el("div", { class: "form-grid" }, limitKeys.map((key) =>
      field(key.replaceAll("_", " "), key, limits[key], key.includes("timeout") || key === "worker_idle" ? "Seconds" : "", "number"))),
    el("div", { class: "form-actions" }, button("Save limits", async () => {
        const changes = Object.fromEntries(limitKeys.map((key) => [key, Number(document.getElementById(key).value)]));
        await act("PATCH", "/config/limits", changes, "Save workload limits?",
          "These values govern concurrent work and worker deadlines. Invalid values will be rejected.");
      }, "primary"))])];
}

function showUnlock() {
  state.data = {}; state.workDetail = null; state.workAttempted = null; state.workLoading = null;
  state.workRequest++; state.replayPlaying = false; state.replayIndex = null;
  for (const id of ["worker-dialog", "confirm-dialog"]) {
    const dialog = document.getElementById(id);
    if (dialog?.open) dialog.close("cancel");
  }
  document.getElementById("worker-detail").replaceChildren();
  document.getElementById("view").replaceChildren();
  document.getElementById("unlock").hidden = false;
  document.getElementById("status").textContent = "Locked";
  document.getElementById("status").className = "connection bad";
}
function render() {
  const data = state.data;
  if (!data.status) return;
  const detailScroll = document.querySelector(".work-main")?.scrollTop || 0;
  const parentScroll = document.querySelector(".parent-pane")?.scrollTop || 0;
  const selected = state.selectedParent;
  const content = state.view === "threads" && state.thread ? (data.detail ? threadDetail(data.detail) : [empty("Loading conversation…")])
    : ({ overview, inbox, threads: threadsView, machines: machinesView,
      activity: activityView, settings: settingsView })[state.view](data);
  document.getElementById("view").replaceChildren(...content.flat().filter(Boolean));
  if (selected === state.selectedParent && state.view === "overview") document.querySelector(".work-main")?.scrollTo(0, detailScroll);
  if (state.view === "overview") {
    document.querySelector(".parent-pane")?.scrollTo(0, parentScroll);
    void loadWorkDetail();
  }
}
async function refresh(force = false) {
  if (state.refreshing) { if (force) state.pending = true; return; }
  if (!force && (document.hidden || !state.auto || document.activeElement?.matches("input, select, textarea") ||
    document.getElementById("confirm-dialog").open)) return;
  state.refreshing = true;
  try {
    const [status, approvals, outbox, threads, jobs, workers, attentionThreads] = await Promise.all([
      api("GET", "/status"), api("GET", "/approvals"), api("GET", "/outbox"),
      api("GET", `/threads?limit=${state.threadLimit}`), api("GET", "/jobs?status=all&limit=200"), api("GET", "/workers?limit=1000"),
      api("GET", "/attention/threads")]);
    const view = state.view, thread = state.thread;
    const extra = view === "threads" && thread ? { detail: await api("GET", "/threads/" + encodeURIComponent(thread)) }
      : view === "machines" || view === "overview" ? { machines: await api("GET", "/machines") }
      : view === "activity" ? { activity: await api("GET", "/activity") }
      : view === "settings" ? { config: await api("GET", "/config") } : {};
    if (view !== state.view || thread !== state.thread) { state.pending = true; return; }
    state.data = { status, approvals, outbox, threads, jobs, workers, attentionThreads, ...extra };
    document.getElementById("status").textContent = status.slack === "connected" ? "Connected" : status.slack;
    document.getElementById("status").className = "connection " + (status.slack === "connected" ? "ok" : "bad");
    document.getElementById("inbox-count").textContent = attention(state.data).length || "";
    state.lastSync = new Date();
    document.getElementById("last-sync").textContent = "Updated " + state.lastSync.toLocaleTimeString();
    document.getElementById("unlock").hidden = true;
    render();
    if (view === "overview" && (state.historyOpen || state.graphMode === "conversation") && state.replayIndex === null) await loadWorkDetail(true);
  } catch (error) {
    if (error.message !== "locked") {
      document.getElementById("status").textContent = "Offline";
      document.getElementById("status").className = "connection bad";
      document.getElementById("last-sync").textContent = state.lastSync ?
        "Last updated " + state.lastSync.toLocaleTimeString() : "Never updated";
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
  document.getElementById("worker-close").addEventListener("click", () => document.getElementById("worker-dialog").close());
  if (state.key) refresh(true);
  setInterval(() => refresh(), 10000);
  setInterval(() => {
    if (!state.replayPlaying || document.hidden || state.view !== "overview") return;
    const events = graph.events(state.workDetail || {});
    if (state.replayIndex >= events.length - 1) state.replayPlaying = false;
    else state.replayIndex++;
    render();
  }, 1200);
}

if (typeof document !== "undefined") start();
if (typeof module !== "undefined") module.exports = { el, markdown, ago, statusPill, filteredThreads, attention, networkSummary, settingsView, overview, latestJobs, workItems, workFlags, parentStatus, workerDetail };
