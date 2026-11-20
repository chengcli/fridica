"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const graph = require("../assets/dashboard/graph.js");

function snapshotAt(detail, at) {
  return graph.snapshot(detail.workers || [], graph.events(detail).filter(e => e.time <= at), at);
}

const detail = {
  session: { id: "thread-1" },
  workers: [
    { id: "w1", session_id: "thread-1", role: "tester", machine: "dungeon3", workspace: "research", created: 5, status: "stopped" },
    { id: "w2", session_id: "thread-1", role: "reviewer", machine: "gpu", workspace: "repo", created: 35, status: "awaiting_approval" },
  ],
  jobs: [
    { id: "failed", worker_id: "w1", queued_at: 10, started_at: 12, finished_at: 20, status: "failed", error: "old failure" },
    { id: "retry", worker_id: "w1", queued_at: 25, started_at: 28, finished_at: 32, status: "done" },
    { id: "current", worker_id: "w1", queued_at: 36, started_at: 38, status: "running" },
    { id: "next", worker_id: "w1", queued_at: 39, status: "queued" },
  ],
  messages: [
    { event_id: "a", ts: "14.2", sender: "U1", text: "Review <@U2> and <@U2>.", source: "slack" },
    { event_id: "b", ts: "33.1", sender: "U2", text: "Done.", source: "slack" },
    { event_id: "a", ts: "14.2", sender: "U1", text: "Review <@U2> and <@U2>.", source: "slack" },
  ],
};

test("replay uses recorded transitions and never shows a worker from the future", () => {
  assert.deepEqual(snapshotAt(detail, 11).map(x => [x.worker.id, x.status]), [["w1", "queued"]]);
  const during = snapshotAt(detail, 15)[0];
  assert.equal(during.status, "running");
  assert.equal(during.worker.status, "running", "current worker state must not overwrite historical job state");
  assert.equal(during.job.finished_at, 0, "future completion is not historical evidence");
  assert.equal(during.job.error, "");
  assert.equal(graph.elapsed(during.job, 15), 3);
  assert.equal(during.observedAt, 15);
  assert.equal(snapshotAt(detail, 22)[0].job.error, "old failure");
  assert.equal(snapshotAt(detail, 33)[0].job.id, "retry");
  assert.equal(snapshotAt(detail, 33)[0].status, "done");
  assert.equal(snapshotAt(detail, 40)[0].job.id, "current");
  assert.equal(detail.workers[0].status, "stopped", "replay must not mutate live records");
});

test("history deduplicates messages and orders equal-time transitions deterministically", () => {
  const events = graph.events(detail);
  assert.equal(events.filter(x => x.kind === "message").length, 2);
  assert(events.every((e, i) => !i || events[i - 1].time <= e.time));
  assert.equal(new Set(events.map(x => x.id)).size, events.length);
  const queued = graph.events({ jobs: [{ id: "same", queued_at: 1, started_at: 1, finished_at: 1, status: "done" }] });
  assert.deepEqual(queued.map(x => x.kind), ["queued", "started", "done"]);
});

test("cancelled jobs retain their outcome and elapsed time stops at the recorded finish", () => {
  const data = { workers: [{ id: "w" }], jobs: [{ id: "j", worker_id: "w", queued_at: 1, started_at: 2, finished_at: 8, status: "cancelled" }] };
  assert.equal(snapshotAt(data, 9)[0].status, "done");
  assert.equal(snapshotAt(data, 9)[0].job.status, "cancelled");
  assert.equal(graph.elapsed(data.jobs[0], 100), 6);
  assert.equal(graph.elapsed(data.jobs[0], 5), 3, "historical runtime stops at the replay timestamp");
  assert.equal(graph.elapsed({ queued_at: 1 }, 100), null);
  assert.deepEqual(snapshotAt({}, 1), []);
  assert.deepEqual(graph.events({}), []);
});

test("missing worker records remain visible and graph layout scales beyond three agents", () => {
  const d = { jobs: [{ id: "j", worker_id: "retired", queued_at: 1, finished_at: 3, status: "done" }] };
  assert.equal(snapshotAt(d, 4)[0].worker.id, "retired");
  const points = graph.positions(24);
  assert.equal(points.length, 24);
  assert.equal(new Set(points.map(p => p.join(","))).size, 24);
  assert(points.every(p => p.every(Number.isFinite)));
});

test("unset zero timestamps do not finish a running job or create epoch events", () => {
  const d = { workers: [{ id: "w", created: 1 }], jobs: [
    { id: "j", worker_id: "w", queued_at: 10, started_at: 12, finished_at: 0, status: "running" }] };
  assert.equal(graph.elapsed(d.jobs[0], 22), 10);
  assert.deepEqual(graph.events(d).map(e => e.kind), ["queued", "started"]);
  assert.equal(graph.elapsed({ started_at: 0, finished_at: 0 }, 22), null);
});

test("cached names resolve mentioned participants and manual aliases retain priority", () => {
  const messages = [
    { ts: "1", sender: "U1", sender_name: "Alice", text: "<@U2> <@U2> <@U3> tell Alice." },
    { ts: "2", sender: "U2", sender_name: "Bob", text: "<@U1|Old name>" },
  ];
  const { nodes, edges } = graph.conversation(messages, "U1");
  assert.deepEqual(edges.map(e => [e.source, e.target, e.count]), [["U1","U2",1], ["U1","U3",1], ["U2","U1",1]]);
  assert.deepEqual(nodes.map(n => [n.id, n.label]), [["U1", "Alice"], ["U2", "Bob"], ["U3", "U3"]]);
  const aliased = graph.conversation(messages, "U1", { U2: "Reviewer" });
  assert.equal(aliased.nodes.find(n => n.id === "U2").label, "Reviewer");
  assert.equal(aliased.nodes[1].tone, nodes[1].tone);
});
