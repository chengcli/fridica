"use strict";
// Evaluate real browser code with only DOM/network boundaries replaced; never send real requests.
const fs = require("node:fs"), vm = require("node:vm"), assert = require("node:assert/strict"), test = require("node:test");
const source = require("node:path").resolve(__dirname, "..");
class Text { constructor(text) { this.text = String(text); } get textContent() { return this.text; } }
class Node {
  constructor(tag) { this.tag = tag; this.children = []; this.attributes = {}; this.className = ""; this.listeners = {}; this.style = {}; this.value = ""; this.open = false; this.returnValue = ""; this.scrollTop = 0; this.disabled = false; }
  append(...children) { this.children.push(...children); if (this.tag === "textarea") this.value = this.textContent; }
  replaceChildren(...children) { this.children = []; this.append(...children); }
  setAttribute(name, value) { this.attributes[name] = String(value); if (["id", "value"].includes(name)) this[name] = String(value); }
  addEventListener(name, fn, options) { (this.listeners[name] ||= []).push({ fn, once: options?.once }); }
  async dispatch(name, event = {}) { const list = [...(this.listeners[name] || [])]; this.listeners[name] = (this.listeners[name] || []).filter(x => !x.once); for (const { fn } of list) await fn({ target: this, currentTarget: this, preventDefault() {}, stopPropagation() {}, ...event }); }
  set textContent(text) { this.children = [new Text(text)]; }
  get textContent() { return this.children.map(x => x.textContent).join(""); }
  scrollTo(_x, y) { this.scrollTop = y; }
  showModal() { this.open = true; }
  close(value) { this.open = false; if (value !== undefined) this.returnValue = value; void this.dispatch("close"); }
  matches(query) { return query.split(",").map(x => x.trim()).includes(this.tag); }
}
const nodes = n => [n, ...n.children.filter(x => x instanceof Node).flatMap(nodes)];
const settle = () => new Promise(resolve => setImmediate(resolve));
function harness() {
  const roots = new Map();
  function add(id, tag = "div", parent) { const n = new Node(tag); n.id = id; if (parent) roots.get(parent).append(n); else roots.set(id, n); return n; }
  ["view", "unlock", "status", "last-sync", "inbox-count", "toast", "current-view", "unlock-form", "key", "refresh-button", "auto-button"].forEach(x => add(x));
  add("confirm-dialog", "dialog"); ["dialog-title", "dialog-body", "dialog-detail", "dialog-confirm"].forEach(x => add(x, "div", "confirm-dialog"));
  add("worker-dialog", "dialog"); add("worker-close", "button", "worker-dialog"); add("worker-detail", "div", "worker-dialog");
  const all = () => [...roots.values()].flatMap(nodes);
  const document = { hidden: false, activeElement: null, createElement: tag => new Node(tag), createElementNS: (_ns, tag) => new Node(tag), createTextNode: text => new Text(text),
    getElementById: id => all().find(x => x.id === id), querySelector: query => all().find(x => x.className.split(" ").includes(query.slice(1))), querySelectorAll: () => [] };
  const requests = [], store = new Map();
  const context = vm.createContext({ console, Node, document, Date, Math, JSON, Map, Set, Promise, Infinity, crypto: { randomUUID: (() => { let i = 0; return () => "fixture-client-" + ++i; })() },
    sessionStorage: { getItem: key => store.get(key) || null, setItem: (key, value) => store.set(key, value) }, localStorage: { getItem: () => null, setItem() {} },
    location: { hash: "", pathname: "/" }, history: { replaceState() {} }, window: { scrollTo() {} },
    setInterval() {}, setTimeout: () => 1, clearTimeout() {},
    fetch: (url, options) => new Promise(resolve => requests.push({ url, options, release: (data, status = 200) => resolve({ status, ok: status < 400, statusText: "fixture error", json: async () => data }) })) });
  vm.runInContext(fs.readFileSync(source + "/assets/dashboard/graph.js", "utf8"), context);
  vm.runInContext(fs.readFileSync(source + "/assets/dashboard/app.js", "utf8").replace('if (typeof document !== "undefined") start();', "") +
    "\nglobalThis.qa = { state, render, chooseParent, loadWorkDetail, overview, graphHistory, instructionPanel, openGuide, showUnlock, refresh, confirmAction, threadDetail };", context);
  return { q: context.qa, document, requests };
}
const parent = (id, status = "working") => ({ id, control: "active", status, summary: id + " task", key: { workspace: "fixture-workspace", channel: "C_FIXTURE", root_ts: "1.0" }, context: {} });
const summary = (parents = [parent("A")]) => ({ status: { owner: "OWNER", slack: "connected" }, threads: parents, workers: [], jobs: [], machines: [], approvals: [], outbox: [], attentionThreads: [] });
const detail = (id, start = 1, count = 200) => ({ session: parent(id), workers: [], jobs: [], instructions: [], messages: Array.from({ length: count }, (_, i) => ({ ts: String(start + i), sender: "U_FIXTURE", text: "event " + (start + i) })) });
const find = (root, predicate) => nodes(root).find(predicate);
const textButton = (root, label) => find(root, n => n.tag === "button" && n.textContent === label);
function seed(h) { h.q.state.data = summary(); h.q.state.selectedParent = "A"; h.q.state.workDetail = detail("A"); h.q.state.workAttempted = "A:200"; }

function releaseBase(h, from) {
  for (const r of h.requests.slice(from)) r.release(r.url === "/api/status" ? summary().status : r.url.startsWith("/api/threads?") ? summary().threads : []);
}
const historyEvent = h => find(h.document.getElementById("view"), n => n.className === "history-message").textContent;
async function guide(h) {
  seed(h); h.q.openGuide(parent("A"));
  const textarea = h.document.getElementById("owner-instruction"); textarea.value = "Fixture instruction"; await textarea.dispatch("input");
  return textButton(h.document.getElementById("worker-detail"), "Send instruction");
}
async function submit(h, send) {
  const pending = send.dispatch("click"); await settle(); h.document.getElementById("confirm-dialog").close("confirm"); await settle(); return { pending };
}

test("delayed A/B/A details cannot overwrite the final selected parent", async () => {
  const h = harness(); h.q.state.data = summary([parent("A"), parent("B")]);
  h.q.chooseParent("A"); h.q.chooseParent("B"); h.q.chooseParent("A");
  h.requests[1].release(detail("B", 9, 1)); await settle(); h.requests[0].release(detail("A", 3, 1)); await settle();
  assert.equal(h.q.state.workDetail, null);
  h.requests[2].release(detail("A", 7, 1)); await settle(); assert.equal(h.q.state.workDetail.messages[0].ts, "7");
});

test("a detail response received in another tab is available on returning to Work", async () => {
  const h = harness(); h.q.state.data = summary(); h.q.chooseParent("A");
  h.q.state.view = "threads"; h.requests[0].release(detail("A", 7, 1)); await settle();
  h.q.state.view = "overview"; h.q.render();
  assert.equal(h.q.state.workDetail?.messages[0].ts, "7");
  assert.equal(h.requests.length, 1);
});

test("Load older during detail polling eventually requests and displays the larger page", async () => {
  const h = harness(); seed(h); h.q.state.historyOpen = true;
  const pending = h.q.loadWorkDetail(true); h.q.render();
  const click = textButton(h.document.getElementById("view"), "Load older messages").dispatch("click");
  h.requests[0].release(detail("A")); await pending; await settle();
  const older = h.requests.find(r => r.url === "/api/threads/A?limit=400");
  assert(older, "the older-page intent must survive an in-flight 200-message poll");
  older.release(detail("A", 1, 400)); await click; await settle(); assert.equal(h.q.state.workDetail.messages.length, 400);
});

test("loading older messages while entering replay keeps the selected event", async () => {
  const h = harness(); seed(h); h.q.state.workDetail = detail("A", 201, 200);
  h.q.state.workLimit = 400;
  const pending = h.q.loadWorkDetail(true);
  h.q.state.historyOpen = true; h.q.state.replayIndex = 100; h.q.render();
  assert(historyEvent(h).includes("event 301"));
  h.requests[0].release(detail("A", 1, 400)); await pending;
  assert(historyEvent(h).includes("event 301"));
});

test("List replay shows recorded workers with no live stop or interrupt controls", async () => {
  const h = harness(); seed(h); h.q.state.graphMode = "list"; h.q.state.replayIndex = 2;
  h.q.state.data.threads[0].pause_reason = "Current pause";
  const worker = { id: "W", session_id: "A", status: "running", role: "tester", machine: "dungeon3", created: 1 };
  h.q.state.workDetail = detail("A", 1, 1); h.q.state.workDetail.workers = [worker];
  h.q.state.workDetail.jobs = [{ id: "old", worker_id: "W", status: "done", queued_at: 1, started_at: 2, finished_at: 3, brief: "Recorded job" }];
  h.q.state.data.workers = [worker]; h.q.state.data.jobs = [{ id: "live", worker_id: "W", status: "running", queued_at: 500, started_at: 501, brief: "Job from future" }];
  h.q.render(); const root = h.document.getElementById("view");
  assert(!root.textContent.includes("Job from future")); assert(root.textContent.includes("Recorded job"));
  await find(root, n => n.className === "work-worker").dispatch("click");
  const modal = h.document.getElementById("worker-detail");
  assert(!modal.textContent.includes("Current pause"));
  assert(!textButton(modal, "Stop")); assert(!textButton(modal, "Interrupt")); assert.equal(h.requests.length, 0);
});

test("each replay step reveals only events through the selected row", () => {
  const h = harness(); seed(h); h.q.state.historyOpen = true;
  const d = h.q.state.workDetail = detail("A", 1, 0);
  d.messages = [{ event_id: "m1", ts: "5.000000", sender: "U1", text: "First message" },
    { event_id: "m2", ts: "5.000000", sender: "U2", text: "Second message" }];
  d.jobs = [{ id: "job", worker_id: "W", queued_at: 5, started_at: 5, finished_at: 5, status: "failed", error: "failure detail" }];
  h.q.state.data.workers = ["W", "later"].map(id => ({ id, session_id: "A" }));
  const root = h.document.getElementById("view");
  for (const [index, status] of [[0, "queued"], [3, "running"], [4, "blocked"]]) {
    h.q.state.replayIndex = index; h.q.render();
    assert(find(root, n => n.className === "head-count").textContent.includes("1 total worker"));
    const worker = find(root, n => n.attributes["data-node"] === "W");
    assert(worker.attributes.class.split(" ").includes(status));
    assert.equal(root.textContent.includes("failure detail"), index === 4);
  }
  h.q.state.graphMode = "conversation"; h.q.state.replayIndex = 1; h.q.render();
  assert(find(root, n => n.attributes["data-node"] === "U1"));
  assert(!find(root, n => n.attributes["data-node"] === "U2"));
});

test("Escape cancels a confirmation even after a previous accepted action", async () => {
  const h = harness(), dialog = h.document.getElementById("confirm-dialog");
  const first = h.q.confirmAction("First", "Fixture"); dialog.close("confirm"); assert.equal(await first, true);
  const second = h.q.confirmAction("Second", "Escape should cancel");
  // The HTML close watcher supplies no new return value on Escape.
  await dialog.dispatch("cancel"); dialog.close(); assert.equal(await second, false);
});

test("locking closes and clears any open worker or guide dialog", () => {
  const h = harness(); seed(h); h.q.openGuide(parent("A")); h.q.showUnlock();
  assert.equal(h.document.getElementById("worker-dialog").open, false);
  assert.equal(h.document.getElementById("worker-detail").textContent, "");
});

test("successful guide instruction clears its input or closes the guide modal", async () => {
  const h = harness(), send = await guide(h), { pending } = await submit(h, send);
  h.requests[0].release({}); await settle(); releaseBase(h, 1); await settle(); h.requests[8].release([]); await pending;
  const dialog = h.document.getElementById("worker-dialog"), input = h.document.getElementById("owner-instruction");
  assert(!dialog.open || !input?.value, "sent text must not remain ready for an accidental duplicate instruction");
});

test("sending a guide instruction preserves text typed while the request is pending", async () => {
  const h = harness(), send = await guide(h), { pending } = await submit(h, send);
  const input = h.document.getElementById("owner-instruction");
  input.value = "Next instruction"; await input.dispatch("input");
  h.requests[0].release({}); await settle(); releaseBase(h, 1); await settle(); h.requests[8].release([]); await pending;
  assert.equal(h.q.state.instructionDraft.text, "Next instruction");
  assert.equal(input.value, "Next instruction");
});

test("retrying an uncertain instruction preserves the idempotency key", async () => {
  const h = harness(), send = await guide(h), first = await submit(h, send);
  const original = JSON.parse(h.requests[0].options.body);
  h.requests[0].release({ error: "fixture timeout" }, 502); await first.pending;
  const retry = await submit(h, send), body = JSON.parse(h.requests[1].options.body);
  assert.equal(body.client_id, original.client_id); assert.equal(body.text, "Fixture instruction");
  h.requests[1].release({ error: "fixture timeout" }, 502); await retry.pending;
});

test("participant detail during replay excludes messages after the selected event", async () => {
  const h = harness(); seed(h); h.q.state.graphMode = "conversation"; h.q.state.historyOpen = true;
  h.q.state.workDetail = detail("A", 1, 3); h.q.state.replayIndex = 0; h.q.render();
  const participant = find(h.document.getElementById("view"), n => n.attributes["data-node"] === "U_FIXTURE"); assert(participant);
  await participant.dispatch("click");
  const modal = h.document.getElementById("worker-detail").textContent; assert(modal.includes("event 1")); assert(!modal.includes("event 3"));
});

test("history and conversation messages show cached sender names with ID fallback", () => {
  const h = harness(); seed(h);
  const d = detail("A", 1, 2);
  d.messages[0].sender_name = "Alice <script>";
  d.messages[1].sender = "U_UNKNOWN";
  const history = h.q.graphHistory(parent("A"), d, require("../assets/dashboard/graph.js").events(d));
  assert(history.textContent.includes("Alice <script>"));
  const conversation = h.q.threadDetail(d).map(n => n.textContent).join(" ");
  assert(conversation.includes("Alice <script>"));
  assert(conversation.includes("U_UNKNOWN"));
  assert(!conversation.includes("Member 1"));
});
