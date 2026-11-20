"use strict";

var FridicaGraph = (() => {
  function number(value) {
    const time = Number(value);
    return Number.isFinite(time) && time > 0 ? time : null;
  }
  const rank = status => status === "running" ? 2 : status === "queued" ? 1 : 0;
  const palette = id => [...String(id)].reduce((n, c) => (n * 31 + c.charCodeAt(0)) >>> 0, 7) % 6;
  const done = status => ["done", "complete", "cancelled", "interrupted", "stopped"].includes(status);
  const state = status => ["failed", "blocked", "lost"].includes(status) ? "blocked" : done(status) ? "done" :
    ["running", "queued"].includes(status) ? status : "idle";

  function preferJob(job, previous) {
    if (!previous) return true;
    const priority = rank(job.status) - rank(previous.status);
    return priority ? priority > 0 : (job.queued_at || 0) > (previous.queued_at || 0);
  }

  function elapsed(job, now = Date.now() / 1000) {
    const start = number(job?.started_at);
    return start === null ? null : Math.max(0, Math.min(number(job.finished_at) ?? now, now) - start);
  }

  function events(detail) {
    const items = new Map();
    const add = item => items.set(item.id, item);
    for (const message of detail.messages || []) {
      const time = number(message.ts);
      if (time !== null) add({ id: "message:" + (message.event_id || message.ts + ":" + message.sender),
        time, order: 1, kind: "message", text: message.text || "[No text stored]", message });
    }
    for (const job of detail.jobs || []) {
      for (const [field, kind, order] of [["queued_at", "queued", 0], ["started_at", "started", 2],
        ["finished_at", job.status || "finished", 3]]) {
        const time = number(job[field]);
        if (time !== null) add({ id: job.id + ":" + field, time, order, kind,
          text: kind === "failed" ? job.error || job.brief : job.brief || job.role || "Worker job", job });
      }
    }
    return [...items.values()].sort((a, b) => a.time - b.time || a.order - b.order || a.id.localeCompare(b.id));
  }

  function snapshot(records, history, at) {
    const jobs = new Map(), latest = new Map();
    for (const { job, kind } of history) {
      if (!job) continue;
      const status = kind === "started" ? "running" : kind;
      const active = status === "queued" || status === "running";
      jobs.set(job.id, { ...job, status, started_at: status === "queued" ? 0 : job.started_at,
        finished_at: active ? 0 : job.finished_at, error: active ? "" : job.error, result: active ? null : job.result });
    }
    for (const job of jobs.values()) if (preferJob(job, latest.get(job.worker_id))) latest.set(job.worker_id, job);
    const workers = new Map(records.filter(w => (number(w.created) !== null && w.created <= at) ||
      latest.has(w.id)).map(w => [w.id, w]));
    for (const [id, job] of latest) if (!workers.has(id)) workers.set(id, { id, role: job.role || "Worker", machine: "", workspace: "" });
    return [...workers.values()].map(worker => {
      const job = latest.get(worker.id);
      return { worker: { ...worker, status: job?.status || "idle", summary: job?.brief || "",
        updated: job?.finished_at || job?.started_at || job?.queued_at || worker.created }, job, status: state(job?.status), observedAt: at };
    });
  }

  function conversation(messages, owner, aliases = {}) {
    const nodes = new Map(), links = new Map(), seen = new Set();
    const visible = [...(messages || [])].filter(m => number(m.ts) !== null)
      .sort((a, b) => Number(a.ts) - Number(b.ts));
    const names = new Map(visible.filter(m => m.sender_name).map(m => [m.sender, m.sender_name]));
    const person = (id, label) => {
      if (!nodes.has(id)) nodes.set(id, { id, label: aliases[id] || names.get(id) || label || (id === owner ? "You" : id),
        tone: palette(id), count: 0 });
      return nodes.get(id);
    };
    let current = null;
    for (const message of visible) {
      const key = message.event_id || message.ts + ":" + message.sender;
      if (seen.has(key)) continue;
      seen.add(key);
      const sender = message.sender || (message.source === "self" ? owner : "unknown");
      person(sender, message.sender_name).count++;
      const targets = new Set();
      for (const tag of (message.text || "").matchAll(/<@([A-Z0-9]+)(?:\|([^>]+))?>/g)) {
        person(tag[1], tag[2]);
        if (tag[1] === sender || targets.has(tag[1])) continue;
        targets.add(tag[1]);
        const key = sender + ":" + tag[1];
        links.set(key, { source: sender, target: tag[1], count: (links.get(key)?.count || 0) + 1 });
      }
      current = { message, sender, targets };
    }
    return { nodes: [...nodes.values()], edges: [...links.values()].map(e => ({ ...e,
      active: current?.sender === e.source && current.targets.has(e.target) })), current };
  }

  function positions(count) {
    if (count === 1) return [[235, 0]];
    if (count === 2) return [[-235, 0], [235, 0]];
    if (count === 3) return [[-255, -100], [0, 170], [255, -100]];
    if (count === 4) return [[-265, -115], [265, -115], [-265, 145], [265, 145]];
    if (count <= 12) return Array.from({ length: count }, (_, i) => {
      const angle = i * Math.PI * 2 / count - Math.PI / 2, radius = Math.max(270, count * 32);
      return [Math.round(Math.cos(angle) * radius), Math.round(Math.sin(angle) * radius * .68)];
    });
    return Array.from({ length: count }, (_, i) => {
      const ring = Math.floor(i / 8), inRing = Math.min(8, count - ring * 8);
      const angle = (i % 8) * Math.PI * 2 / inRing - Math.PI / 2;
      const radius = 285 + ring * 230;
      return [Math.round(Math.cos(angle) * radius), Math.round(Math.sin(angle) * radius * .65)];
    });
  }

  function draw({ nodes, edges, select, viewport, onViewport }) {
    const make = (tag, attrs = {}, ...children) => {
      const node = document.createElementNS("http://www.w3.org/2000/svg", tag);
      for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
      for (const child of children.flat()) node.append(typeof child === "string" ? document.createTextNode(child) : child);
      return node;
    };
    const root = document.createElement("div");
    root.className = "orbit-stage";
    const svg = make("svg", { class: "orbit-map", role: "group", "aria-label": "Agent relationships" });
    const centered = nodes[0]?.kind === "parent";
    const points = positions(nodes.length - Number(centered));
    const places = new Map(nodes.map((node, i) => [node.id, centered && !i ? [0, 0] : points[i - Number(centered)]]));
    const xy = [...places.values()];
    const bounds = xy.length ? [Math.min(...xy.map(p => p[0])) - 130, Math.min(...xy.map(p => p[1])) - 90,
      Math.max(...xy.map(p => p[0])) + 130, Math.max(...xy.map(p => p[1])) + 140] : [-300, -200, 300, 200];
    const fit = { x: bounds[0], y: bounds[1], w: Math.max(650, bounds[2] - bounds[0]), h: Math.max(430, bounds[3] - bounds[1]) };
    fit.x -= (fit.w - (bounds[2] - bounds[0])) / 2;
    fit.y -= (fit.h - (bounds[3] - bounds[1])) / 2;
    let box = viewport ? { ...viewport } : { ...fit };
    const paint = () => { svg.setAttribute("viewBox", [box.x, box.y, box.w, box.h].join(" ")); onViewport?.({ ...box }); };
    svg.setAttribute("viewBox", [box.x, box.y, box.w, box.h].join(" "));
    const defs = make("defs");
    const colors = [["#e1c191", "#5a442c"], ["#a2cadc", "#244c66"], ["#d4dcdb", "#4b5c6b"],
      ["#e0ad91", "#6d4240"], ["#bab6de", "#514c7b"], ["#a9cdb8", "#386159"]];
    const prefix = "orb-" + (++draw.sequence) + "-";
    colors.forEach(([light, dark], i) => defs.append(make("radialGradient", { id: prefix + i, cx: ".27", cy: ".22", r: ".82" },
      make("stop", { offset: "0", "stop-color": light }), make("stop", { offset: ".68", "stop-color": dark }),
      make("stop", { offset: "1", "stop-color": "#03090f" }))));
    defs.append(make("marker", { id: prefix + "arrow", viewBox: "0 0 10 10", refX: "8", refY: "5",
      markerWidth: "5", markerHeight: "5", orient: "auto" },
      make("path", { d: "M1 1 L8 5 L1 9", fill: "none", stroke: "#9bbab6", "stroke-width": "1.3" })));
    svg.append(defs);
    const field = make("g", { "aria-hidden": "true", "pointer-events": "none" });
    for (let i = 0; i < 52; i++) field.append(make("circle", {
      cx: fit.x + ((i * 173.7 + 41) % fit.w), cy: fit.y + ((i * 113.3 + 17) % fit.h),
      r: i % 9 ? ".65" : "1.1", fill: "#bbd1d5", opacity: i % 9 ? ".18" : ".4" }));
    for (const r of [170, 290, 410]) field.append(make("ellipse", { class: "orbit-guide",
      rx: r, ry: r * .68, "stroke-dasharray": r === 290 ? "2 8" : "none" }));
    svg.append(field);
    const radius = node => node.kind === "parent" ? 48 : node.kind === "person" ? 38 : 34;
    const nodeById = new Map(nodes.map(node => [node.id, node]));
    for (const edge of edges) {
      if (!places.has(edge.source) || !places.has(edge.target)) continue;
      const [x, y] = places.get(edge.source), [u, v] = places.get(edge.target);
      const distance = Math.hypot(u - x, v - y) || 1, dx = (u - x) / distance, dy = (v - y) / distance;
      const a = radius(nodeById.get(edge.source)) + 11, b = radius(nodeById.get(edge.target)) + 14;
      const bend = Math.min(38, distance * .12);
      svg.append(make("path", { class: "orbit-link" + (edge.active ? " active" : ""),
        "marker-end": "url(#" + prefix + "arrow)",
        d: "M"+(x+dx*a)+" "+(y+dy*a)+" Q"+((x+u)/2-dy*bend)+" "+((y+v)/2+dx*bend)+" "+(u-dx*b)+" "+(v-dy*b) }));
    }
    for (const node of nodes) {
      const [x, y] = places.get(node.id), r = radius(node);
      const group = make("g", { class: "orbit-node " + (node.status || "idle") + (node.active ? " selected" : ""),
        transform: "translate("+x+" "+y+")", role: "button", tabindex: "0", "data-node": node.id,
        "aria-label": [node.label, node.subtitle, node.meta, node.status].filter(Boolean).join(". ") });
      group.append(make("title", {}, [node.label, node.subtitle, node.meta].filter(Boolean).join("\n")),
        make("circle", { class: "orbit-halo", r: r + 15 }),
        make("circle", { class: "orbit-ring", r: r + 8 }),
        make("circle", { r: r + 1, cy: 3, fill: "#000", opacity: ".45" }),
        make("circle", { r, fill: "url(#"+prefix+(node.tone ?? palette(node.id))+")" }),
        make("circle", { r, fill: "none", stroke: "#dce7da", "stroke-width": ".7", opacity: ".18" }));
      const tone = node.tone ?? palette(node.id);
      if (tone === 2) {
        for (const [cx, cy, cr] of [[-.35,-.2,.15],[.25,.3,.2],[-.1,.45,.09]])
          group.append(make("circle", { cx: cx*r, cy: cy*r, r: cr*r, fill: "#202d42", opacity: ".25" }));
      } else if (tone === 1 || tone === 5) {
        group.append(make("path", { d: "M"+(-.5*r)+" "+(-.4*r)+"q"+(.5*r)+" "+(-.4*r)+" "+(.65*r)+" "+(.1*r)+"t"+(-.1*r)+" "+(.6*r)+"q"+(-.3*r)+" "+(.2*r)+" "+(-.25*r)+" "+(-.3*r)+"Z",
          fill: "#94a986", opacity: ".3" }));
      } else {
        for (const dy of [-.5,-.28,.05,.3,.5]) {
          const width = Math.sqrt(1-dy*dy)*r*.94;
          group.append(make("path", { d: "M"+(-width)+" "+(dy*r)+"q"+width+" "+(r*.07)+" "+(width*2)+" 0",
            stroke: "#e3c3a3", "stroke-width": r*.09, opacity: ".16", fill: "none" }));
        }
      }
      group.append(make("text", { class: "orbit-name", y: r + 29 }, String(node.label || "Agent").slice(0, 32)));
      if (node.subtitle) group.append(make("text", { class: "orbit-task", y: r + 51 }, node.subtitle.slice(0, 48)));
      if (node.meta) group.append(make("text", { class: "orbit-meta", y: r + 72 }, node.meta.slice(0, 48)));
      if (node.age) group.append(make("text", { class: "orbit-age", y: r + 91 }, node.age));
      group.addEventListener("click", () => select(node));
      group.addEventListener("keydown", event => {
        if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(node); }
      });
      svg.append(group);
    }
    const controls = document.createElement("div");
    controls.className = "orbit-zoom";
    const zoom = factor => {
      const next = Math.max(0.2, Math.min(6, fit.w / box.w * factor));
      const w = fit.w / next, h = fit.h / next;
      box = { x: box.x + (box.w - w) / 2, y: box.y + (box.h - h) / 2, w, h };
      paint();
    };
    for (const [label, name, run] of [["−", "Zoom out", () => zoom(1 / 1.3)], ["Fit", "Fit all agents", () => { box = { ...fit }; paint(); }],
      ["+", "Zoom in", () => zoom(1.3)]]) {
      const button = document.createElement("button");
      button.textContent = label; button.setAttribute("aria-label", name);
      button.addEventListener("click", run); controls.append(button);
    }
    let drag = null;
    svg.addEventListener("pointerdown", event => {
      if (event.target.closest("[data-node]")) return;
      drag = { x: event.clientX, y: event.clientY, box: { ...box } };
      svg.setPointerCapture(event.pointerId);
    });
    svg.addEventListener("pointermove", event => {
      if (!drag) return;
      const rect = svg.getBoundingClientRect();
      box = { ...box, x: drag.box.x - (event.clientX - drag.x) * box.w / rect.width,
        y: drag.box.y - (event.clientY - drag.y) * box.h / rect.height }; paint();
    });
    for (const event of ["pointerup", "pointercancel"]) svg.addEventListener(event, () => { drag = null; });
    root.append(svg, controls);
    return root;
  }
  draw.sequence = 0;
  return { preferJob, state, elapsed, events, snapshot, conversation, positions, draw };
})();

if (typeof module !== "undefined") module.exports = FridicaGraph;
