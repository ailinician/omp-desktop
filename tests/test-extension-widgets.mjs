#!/usr/bin/env node
// Regression script for src/app/extension-widgets.js — omp extension status
// lines and text widgets in the rail (#52): set / replace / remove by key,
// terminal escapes stripped, bounds shared with src-tauri/src/agent/ext_ui.rs,
// the backend snapshot, and the cards and Tweaks rows built from the state.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/extension-widgets.js"), "utf8"))(win);
const X = win.OMP_EXT_WIDGETS;

let passed = 0;
function check(label, fn) {
  try { fn(); } catch (err) { console.error(`FAILED: ${label}`); throw err; }
  passed++;
}
const status = (key, text) => ({ type: "extension_ui_request", id: "1", method: "setStatus", statusKey: key, ...(text === undefined ? {} : { statusText: text }) });
const widget = (key, lines) => ({ type: "extension_ui_request", id: "2", method: "setWidget", widgetKey: key, ...(lines === undefined ? {} : { widgetLines: lines }) });
const run = (...frames) => frames.reduce(X.apply, X.EMPTY);

check("set, replace in place, remove (absent text or lines)", () => {
  let ext = run(status("plan-mode", "⏸ plan"), status("review", "Loading…"), status("plan-mode", "📋 1/3"));
  assert.deepEqual(ext.statuses, [{ key: "plan-mode", text: "📋 1/3" }, { key: "review", text: "Loading…" }]);
  ext = X.apply(ext, status("plan-mode"));
  assert.deepEqual(ext.statuses, [{ key: "review", text: "Loading…" }]);
  ext = X.apply(ext, widget("todos", ["☐ a", "☑ b"]));
  assert.deepEqual(ext.widgets, [{ key: "todos", lines: ["☐ a", "☑ b"], more: 0 }]);
  ext = X.apply(ext, widget("todos"));
  assert.deepEqual(ext.widgets, []);
});
check("statuses and widgets are separate namespaces", () => {
  const ext = run(status("x", "s"), widget("x", ["w"]), status("x"));
  assert.deepEqual([ext.statuses, ext.widgets.map(w => w.key)], [[], ["x"]]);
});
check("unrelated, unchanged and malformed frames return the same state object", () => {
  const ext = run(status("keep", "v"), widget("w", ["l"]));
  for (const frame of [
    null, { type: "agent_end" },
    { type: "extension_ui_request", method: "notify", message: "hi" },
    status("", "empty key"), status("k".repeat(201), "long key"), { ...status("x", "y"), statusKey: 5 },
    status("missing"), widget("missing"),
  ]) {
    assert.equal(X.apply(ext, frame), ext, JSON.stringify(frame));
  }
  assert.deepEqual(X.apply(ext, widget("w", "not an array")).widgets, [], "a non-array widget is a removal, as in the backend");
});
check("a key's UTF-8 length is bounded like the backend's", () => {
  assert.equal(run(status("é".repeat(100), "ok")).statuses.length, 1, "200 bytes");
  assert.equal(run(status("é".repeat(101), "no")).statuses.length, 0, "202 bytes");
});
check("terminal escapes are stripped: SGR, OSC 8 links keep their text, cursor moves, controls", () => {
  const sgr = "\u001b[32m☑ \u001b[39m\u001b[2mdone\u001b[22m";
  const link = "see \u001b]8;;https://x.y\u001b\\the docs\u001b]8;;\u001b\\ now";
  const linkBel = "\u001b]8;;https://x.y\u0007here\u001b]8;;\u0007";
  const misc = "a\u001b[2Kb\u001b(Bc\rd\u0007e\u009b31mf\tg";
  assert.deepEqual(run(widget("w", [sgr, link, linkBel, misc])).widgets[0].lines,
    ["☑ done", "see the docs now", "here", "abcde" + "f\tg"]);
  assert.equal(run(status("s", "\u001b[33m⏸ plan\u001b[39m")).statuses[0].text, "⏸ plan");
});
check("lines, line length and keys are bounded; non-string lines skipped", () => {
  const ext = run(widget("w", Array.from({ length: 70 }, (_, i) => `l${i}`)));
  assert.equal(ext.widgets[0].lines.length, 50);
  assert.equal(ext.widgets[0].more, 20);
  assert.deepEqual(run(widget("m", ["a", 1, null, "b"])).widgets[0].lines, ["a", "b"]);
  const long = run(status("s", "x".repeat(600))).statuses[0].text;
  assert.equal(long.length, 500);
  assert.ok(long.endsWith("…"));
  const emoji = run(status("s", "x".repeat(498) + "😀😀")).statuses[0].text;
  assert.ok(!/[\ud800-\udbff]…$/.test(emoji), "never ends on half a surrogate pair");
  let keys = X.EMPTY;
  for (let i = 0; i < 40; i++) keys = X.apply(keys, status(`k${i}`, "v"));
  assert.equal(keys.statuses.length, 32, "keys past the bound are ignored");
  keys = X.apply(X.apply(keys, status("k0")), status("late", "v"));
  assert.equal(keys.statuses.at(-1).key, "late", "room again after a removal");
});
check("fromSnapshot normalises like live frames and carries the backend's line count", () => {
  const ext = X.fromSnapshot({
    statuses: [{ key: "plan-mode", text: "\u001b[33m⏸ plan\u001b[39m" }, { key: "", text: "bad" }],
    widgets: [{ key: "todos", lines: ["☐ a"], more: 0 }, { key: "big", lines: ["x"], more: 70 }, { key: "bad", lines: "x" }],
  });
  assert.deepEqual(ext.statuses, [{ key: "plan-mode", text: "⏸ plan" }]);
  assert.deepEqual(ext.widgets, [{ key: "todos", lines: ["☐ a"], more: 0 }, { key: "big", lines: ["x"], more: 70 }]);
  for (const bad of [null, undefined, {}, { statuses: "x", widgets: 5 }]) assert.deepEqual(X.fromSnapshot(bad), X.EMPTY);
});
check("only methods omp never waits on are fire-and-forget", () => {
  for (const m of ["notify", "setStatus", "setWidget", "setTitle", "set_editor_text"]) assert.equal(X.isFireAndForget(m), true, m);
  for (const m of ["select", "confirm", "input", "editor", "ask", "open_url", "cancel", undefined]) assert.equal(X.isFireAndForget(m), false, String(m));
});
check("cards: one status card with key/value rows, then a mono text card per widget with lines", () => {
  const ext = run(widget("plan-todos", ["☑ a", "☐ b"]), status("plan-mode", "📋 1/2"), widget("empty", []),
    widget("big", Array.from({ length: 51 }, () => "x")));
  const cards = X.cards(ext);
  assert.deepEqual(cards.map(c => [c.id, c.title, c.tweak]), [
    ["ext-status:", "status", "widget:ext-status"],
    ["ext:plan-todos", "plan-todos", "widget:ext:plan-todos"],
    ["ext:big", "big", "widget:ext:big"],
  ], "a widget without lines has no card");
  assert.deepEqual(cards[0].run.widget.blocks, [{ type: "kv", rows: [{ k: "plan-mode", v: "📋 1/2" }] }]);
  assert.deepEqual(cards[1].run.widget.blocks, [{ type: "text", text: "☑ a\n☐ b", mono: true }]);
  assert.ok(cards[2].run.widget.blocks[0].text.endsWith("\n… 1 more line"));
  assert.deepEqual([cards[0].run.state, cards[0].run.durationMs, cards[0].run.widget.status], ["ok", null, "neutral"]);
  assert.deepEqual(X.cards(X.EMPTY), []);
});
check("toggles list the status card and every published widget key", () => {
  assert.deepEqual(X.toggles(run(status("s", "v"), widget("w", []))), [
    { tweak: "widget:ext-status", label: "extension: status lines" },
    { tweak: "widget:ext:w", label: "extension: w" },
  ]);
  assert.deepEqual(X.toggles(run(widget("w", ["x"]))).map(t => t.tweak), ["widget:ext:w"]);
});

console.log(`test-extension-widgets: ${passed} checks passed`);
