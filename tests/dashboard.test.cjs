"use strict";
// Unit tests for the dashboard's rendering helpers, with a minimal DOM stand-in.
const test = require("node:test");
const assert = require("node:assert");

class Node {
  constructor(tag) { this.tag = tag; this.children = []; this.attributes = {}; this.className = ""; this.listeners = {}; }
  append(child) { this.children.push(child); }
  setAttribute(name, value) { this.attributes[name] = value; }
  addEventListener(name, handler) { this.listeners[name] = handler; }
  get textContent() { return this.children.map((child) => (child instanceof Node ? child.textContent : child.text)).join(""); }
}
class Text { constructor(text) { this.text = text; } }
global.Node = Node;
global.document = undefined;
const documentStub = { createElement: (tag) => new Node(tag), createTextNode: (text) => new Text(text) };

const app = (() => { global.document = undefined; const exported = require("../src/fridica/dashboard/static/app.js"); return exported; })();
global.document = documentStub;

test("el builds nested nodes and skips empty children", () => {
  const node = app.el("div", { class: "card", title: "t" }, "a", null, false, ["b", app.el("span", {}, "c")]);
  assert.strictEqual(node.className, "card");
  assert.strictEqual(node.attributes.title, "t");
  assert.strictEqual(node.textContent, "abc");
});

test("long text renders basic Markdown as safe DOM", () => {
  const rich = app.markdown("## Review\n\n- **Passed** `check`\n- [PR](https://github.com/chengcli/fridica)\n\n```\n<unsafe>\n```");
  const nodes = (node) => [node, ...node.children.filter((child) => child instanceof Node).flatMap(nodes)];
  const all = nodes(rich);
  for (const tag of ["h3", "ul", "li", "strong", "code", "a", "pre"]) assert(all.some((node) => node.tag === tag), tag);
  assert.strictEqual(all.find((node) => node.tag === "a").attributes.href, "https://github.com/chengcli/fridica");
  assert(rich.textContent.includes("<unsafe>"));
  assert(!all.some((node) => node.tag === "unsafe"));
  const blocked = app.markdown("[click](javascript:alert(1)) <img src=x>");
  assert(!nodes(blocked).some((node) => ["a", "img"].includes(node.tag)));
  assert(blocked.textContent.includes("[click](javascript:alert(1)) <img src=x>"));
  assert.strictEqual(app.markdown("# ").textContent, "# ");
});

test("statusPill picks a tone", () => {
  assert.strictEqual(app.statusPill("failed").className, "pill bad");
  assert.strictEqual(app.statusPill("complete").className, "pill ok");
  assert.strictEqual(app.statusPill("mystery").className, "pill ");
});

test("ago is relative and coarse", () => {
  const now = Date.now() / 1000;
  assert.strictEqual(app.ago(now - 30), "30s ago");
  assert.strictEqual(app.ago(now - 600), "10m ago");
  assert.strictEqual(app.ago(now - 7200), "2h ago");
  assert.strictEqual(app.ago(0), "");
});

test("thread filters use context and control state", () => {
  const threads = [
    { control: "active", summary: "Review figures", context: { repo: "snapy", branch: "main" } },
    { control: "paused", summary: "Waiting for data", context: { repo: "couple", branch: "test" } },
  ];
  assert.deepStrictEqual(app.filteredThreads(threads, "active", "snapy"), [threads[0]]);
  assert.deepStrictEqual(app.filteredThreads(threads, "paused", "main"), []);
  assert.deepStrictEqual(app.filteredThreads(threads, "all", "test"), [threads[1]]);
});

test("inbox counts approvals, paused threads, and unconfirmed posts", () => {
  const items = app.attention({
    approvals: [{ id: "a1", session_id: "t1", summary: "Run command", kind: "command", created: 1 }],
    threads: [{ id: "t2", control: "paused", status: "waiting", summary: "Review", context: {}, updated: 1 }],
    outbox: [{ id: 1, session_id: "t1", state: "ambiguous", text: "Report" }],
  });
  assert.deepStrictEqual(items.map((item) => item.type), ["approval", "thread", "outbox"]);
  assert.strictEqual(items[2].title, "Delivery needs verification");
});

test("access view does not imply Codex enforces a domain list", () => {
  assert.strictEqual(app.networkSummary({ backends: ["codex", "claude"] }, { network: ["github.com"] }),
    "Codex: full network; Claude: github.com");
  assert.strictEqual(app.networkSummary({ backends: ["codex"] }, { network: [] }), "Network off");
});

test("settings omit removed loop limits while retaining Python compatibility", () => {
  const parent = { backend: "claude", model: "", triage_model: "", reasoning_effort: "" };
  const limits = { max_jobs: 4, max_workers_per_thread: 4, max_delegations_per_turn: 3, job_timeout: 60, worker_idle: 30 };
  const inputs = (nodes) => nodes.flatMap((node) => !(node instanceof Node) ? [] : [
    ...(node.tag === "input" ? [node.attributes.id] : []), ...inputs(node.children),
  ]);
  const rust = inputs(app.settingsView({ config: { parent, limits } }));
  assert(rust.includes("max_jobs"));
  assert(!rust.includes("max_wait_replies"));
  assert(!rust.includes("max_no_progress"));
  const python = inputs(app.settingsView({ config: { parent, limits: { ...limits, max_wait_replies: 3, max_no_progress: 3 } } }));
  assert(python.includes("max_wait_replies"));
  assert(python.includes("max_no_progress"));
});

test("work graph shows parent, worker, server and environment; detail explains failure", () => {
  const now = Date.now() / 1000;
  const data = {
    status: {}, approvals: [], outbox: [], attentionThreads: [],
    threads: [{ id: "t1", summary: "Review snapy", status: "working", control: "active", updated: now - 60,
      context: { repo: "snapy" } }],
    workers: [{ id: "w1", session_id: "t1", role: "reviewer", machine: "dungeon3", workspace: "snapy",
      status: "idle", updated: now - 30 }],
    machines: [{ name: "dungeon3", transport: "ssh", workspace_details: [{ name: "snapy", mode: "sandbox" }] }],
    jobs: [{ id: "j1", worker_id: "w1", session_id: "t1", brief: "Check CUDA regression",
      status: "failed", error: "worker_401", queued_at: now - 900, started_at: now - 800, finished_at: now - 120 }],
  };
  const content = app.overview(data).filter(Boolean).map((node) => node.textContent).join(" ");
  for (const expected of ["Review snapy", "Parent", "reviewer", "Check CUDA regression", "dungeon3", "SSH",
    "sandbox", "blocked"]) assert(content.includes(expected), expected);
  assert(!content.includes("worker_401"), "keep full error in worker detail");
  const item = app.workItems(data.threads[0], data, app.latestJobs(data.jobs))[0];
  const detail = app.workerDetail(data.threads[0], item).filter(Boolean).map((node) => node.textContent).join(" ");
  assert(detail.includes("worker_401"));
  assert(detail.includes("dungeon3"));
  assert(!detail.includes("Interrupt") && detail.includes("Stop"), "idle workers cannot be interrupted");
  assert(!content.includes("Running 13m"), "a finished job must not show a running timer");
});

test("worker detail formats task and result", () => {
  const thread = { id: "t1", summary: "Review" };
  const item = { worker: { status: "idle", role: "reviewer" }, job: { status: "done",
    brief: "- **Check** `x1`", result: { summary: "## Result\nPassed" } },
  status: "done", place: { host: "local", environment: "sandbox" } };
  const nodes = app.workerDetail(thread, item).filter(Boolean);
  const descendants = (node) => [node, ...node.children.filter((child) => child instanceof Node).flatMap(descendants)];
  const tags = nodes.flatMap(descendants).map((node) => node.tag);
  for (const tag of ["ul", "strong", "code", "h3"]) assert(tags.includes(tag), tag);
});

test("work graph uses the latest job and keeps completed workers collapsed", () => {
  const data = {
    status: {}, approvals: [], outbox: [], attentionThreads: [],
    threads: [{ id: "t1", summary: "Review", status: "working", control: "active", context: {} }],
    workers: [{ id: "w1", session_id: "t1", role: "tester", machine: "local", workspace: "repo", status: "idle" }],
    machines: [{ name: "local", transport: "local", workspace_details: [] }],
    jobs: [
      { id: "old", worker_id: "w1", session_id: "t1", brief: "Old attempt", status: "failed", error: "old_error", queued_at: 10 },
      { id: "new", worker_id: "w1", session_id: "t1", brief: "Retry passed", status: "done", queued_at: 20 },
    ],
  };
  const content = app.overview(data).filter(Boolean).map((node) => node.textContent).join(" ");
  assert(content.includes("Completed & idle workers · 1"));
  assert(!content.includes("Retry passed"));
  assert(!content.includes("old_error"));
  const item = app.workItems(data.threads[0], data, app.latestJobs(data.jobs))[0];
  assert.strictEqual(item.job.brief, "Retry passed");
  assert.strictEqual(item.status, "done");
});

test("work view shows a running job before later queued jobs on the same worker", () => {
  const data = {
    status: {}, approvals: [], outbox: [], attentionThreads: [],
    threads: [{ id: "t1", summary: "Build", status: "working", control: "active", context: {} }],
    workers: [{ id: "w1", session_id: "t1", role: "builder", machine: "stormy", workspace: "snapy", status: "running" }],
    machines: [{ name: "stormy", transport: "ssh", workspace_details: [] }],
    jobs: [
      { id: "j1", worker_id: "w1", session_id: "t1", brief: "Build GPU", status: "running", queued_at: 10, started_at: 11 },
      { id: "j2", worker_id: "w1", session_id: "t1", brief: "Run tests", status: "queued", queued_at: 20 },
    ],
  };
  const content = app.overview(data).filter(Boolean).map((node) => node.textContent).join(" ");
  assert(content.includes("Build GPU"));
  assert(!content.includes("Last: Run tests"));
});

test("parent filters follow worker state without marking paused work as running", () => {
  const paused = { control: "paused", status: "working" };
  const complete = { control: "active", status: "complete" };
  assert.deepStrictEqual(app.workFlags(paused, [{ status: "done" }]),
    { current: true, running: false, queued: false, blocked: true, waiting: false, completed: false });
  const failed = app.workFlags(complete, [{ status: "blocked" }]);
  assert(!failed.current && !failed.blocked && failed.completed);
  assert.strictEqual(app.parentStatus(complete, failed), "complete");
  assert(app.workFlags(complete, [{ status: "running" }]).running);
  assert(app.workFlags(complete, [{ status: "done" }]).completed);
  const approval = app.workItems({ ...complete, id: "t" }, { workers: [{ id: "w", session_id: "t", status: "awaiting_approval" }],
    jobs: [], machines: [] }, new Map([["w", { status: "running", error: "old_error" }]]))[0];
  assert.strictEqual(approval.status, "blocked");
  const detail = app.workerDetail(complete, approval).filter(Boolean).map((node) => node.textContent).join(" ");
  assert(detail.includes("Approval pending") && !detail.includes("old_error"));
  const queued = app.workerDetail(complete, { worker: { status: "queued" }, job: { status: "queued" },
    status: "queued", place: { host: "local", environment: "local" } }).filter(Boolean).map((node) => node.textContent).join(" ");
  assert(!queued.includes("Interrupt"));
});
