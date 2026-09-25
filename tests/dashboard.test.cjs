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
