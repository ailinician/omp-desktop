#!/usr/bin/env node
// Regression script for src/app/layout-widths.js — widths of the resizable
// columns beside the chat (#41): defaults, saved widths within bounds, the
// fit to a narrow window (the chat keeps CHAT_MIN while the panes can give),
// the bound on a dragged pane and the handle's keyboard steps.
// Run: node tests/test-layout-widths.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/layout-widths.js"), "utf8"))(win);
const L = win.OMP_LAYOUT_WIDTHS;

let passed = 0;
function check(label, fn) {
  try {
    fn();
  } catch (err) {
    console.error(`FAILED: ${label}`);
    throw err;
  }
  passed++;
}

const sum = (o) => Object.values(o).reduce((a, b) => a + b, 0);

check("unsaved panes take their defaults, narrower ones in a narrow window", () => {
  const all = ["sidebar", "split", "rail"];
  assert.deepEqual(L.fit({}, all, 1600), { sidebar: 220, split: 320, rail: 280 });
  assert.deepEqual(L.fit({ sidebar: 0, split: 0, rail: 0 }, all, 1600, { narrow: true }), { sidebar: 188, split: 280, rail: 240 });
});

check("a saved width is used within the pane's bounds", () => {
  assert.deepEqual(L.fit({ sidebar: 300, rail: 400 }, ["sidebar", "rail"], 1600), { sidebar: 300, rail: 400 });
  assert.deepEqual(L.fit({ sidebar: 50, rail: 5000 }, ["sidebar", "rail"], 1600), { sidebar: 160, rail: 520 });
  assert.deepEqual(L.fit({ sidebar: NaN }, ["sidebar"], 1600), { sidebar: 220 }, "NaN is unsaved");
});

check("only open panes get a width", () => {
  assert.deepEqual(L.fit({ sidebar: 300, split: 400 }, ["split"], 1600), { split: 400 });
});

check("the tree defaults to at most half the stage beside the sidebar", () => {
  assert.equal(L.fit({}, ["tree"], 1600).tree, 540);
  assert.equal(L.fit({}, ["sidebar", "tree"], 1100).tree, 440, "(1100 - 220) / 2");
  assert.equal(L.fit({}, ["tree"], 500).tree, 320, "never below its minimum");
  assert.equal(L.fit({ tree: 700 }, ["sidebar", "tree"], 1600).tree, 700, "a saved width is not halved");
});

check("panes give up room in proportion to their slack so the chat keeps CHAT_MIN", () => {
  // 300 + 400 = 700, room for panes 1000 - 420 = 580 → over by 120.
  // Slack: 140 + 160 = 300; each gives 120 / 300 of its slack: 56 and 64.
  const w = L.fit({ sidebar: 300, rail: 400 }, ["sidebar", "rail"], 1000);
  assert.deepEqual(w, { sidebar: 244, rail: 336 });
  assert.ok(1000 - sum(w) >= L.CHAT_MIN);
});

check("a window too narrow even for the minimums keeps them; the chat gets the rest", () => {
  const w = L.fit({}, ["sidebar", "split", "rail"], 960);
  assert.deepEqual(w, { sidebar: 160, split: 240, rail: 240 });
});

check("the reserved column (browser panel) counts against the chat's room", () => {
  const w = L.fit({ split: 500 }, ["split"], 1200, { reserve: 360 });
  assert.equal(w.split, 420, "1200 - 360 - 420");
});

check("before the first measure (total 0) the widths are the preferred ones", () => {
  assert.deepEqual(L.fit({ rail: 500 }, ["rail", "split"], 0), { rail: 500, split: 320 });
});

check("a dragged pane stays within its bounds and the room the chat can spare", () => {
  const widths = { sidebar: 220, rail: 280 };
  assert.equal(L.dragWidth("rail", 350.6, widths, 1600), 351);
  assert.equal(L.dragWidth("rail", 100, widths, 1600), 240, "min");
  assert.equal(L.dragWidth("rail", 900, widths, 1600), 520, "max");
  assert.equal(L.dragWidth("rail", 900, widths, 1000), 360, "1000 - 220 - 420");
  assert.equal(L.dragWidth("rail", 900, widths, 1000, 100), 260, "less a reserved column");
  assert.equal(L.dragWidth("rail", 900, widths, 600), 240, "the pane's minimum wins over the chat's");
  const shown = L.fit({ rail: L.dragWidth("rail", 900, widths, 1100) }, ["sidebar", "rail"], 1100);
  assert.deepEqual(shown, { sidebar: 220, rail: 460 }, "a pane at its limit pushes no other pane");
});

check("a resize saves the layout shown: the panes the window squeezed keep their shown width", () => {
  const open = ["sidebar", "split", "rail"];
  const saved = { sidebar: 420, split: 0, rail: 400 };
  const widths = L.fit(saved, open, 1440);
  assert.deepEqual(widths, { sidebar: 357, split: 300, rail: 361 });
  const patch = L.settle("split", 302, widths, saved, open, 1440);
  assert.deepEqual(patch, { split: 302, sidebar: 357, rail: 361 });
  assert.deepEqual(L.fit({ ...saved, ...patch }, open, 1440), { sidebar: 357, split: 302, rail: 361 }, "nothing moves once saved");
});

check("a resize leaves the panes that would stay as shown alone (a default stays 0)", () => {
  const open = ["sidebar", "tree"];
  const saved = { sidebar: 0, tree: 0 };
  const widths = L.fit(saved, open, 1600);
  assert.deepEqual(widths, { sidebar: 220, tree: 540 });
  assert.deepEqual(L.settle("sidebar", 260, widths, saved, open, 1600), { sidebar: 260 });
});

check("a sidebar resize keeps a default tree, which halves the stage, where it is shown", () => {
  const open = ["sidebar", "tree"];
  const saved = { sidebar: 0, tree: 0 };
  const shown = L.fit(saved, open, 1100);
  assert.equal(shown.tree, 440, "(1100 - 220) / 2");
  const patch = L.settle("sidebar", 200, shown, saved, open, 1100);
  assert.deepEqual(patch, { sidebar: 200, tree: 440 }, "unsaved, it would grow to (1100 - 200) / 2");
  assert.deepEqual(L.fit({ ...saved, ...patch }, open, 1100), { sidebar: 200, tree: 440 }, "nothing moves once saved");
});

check("keys: the arrow pointing away from the pane grows it", () => {
  const now = { value: 300, min: 240, max: 520 };
  assert.equal(L.nudge("ArrowLeft", false, "left", now), 316);
  assert.equal(L.nudge("ArrowRight", false, "left", now), 284);
  assert.equal(L.nudge("ArrowRight", true, "right", now), 364);
  assert.equal(L.nudge("ArrowLeft", true, "right", now), 236, "unclamped: the caller bounds it");
  assert.equal(L.nudge("Home", false, "left", now), 240);
  assert.equal(L.nudge("End", false, "right", now), 520);
  assert.equal(L.nudge("Enter", false, "left", now), "reset");
  assert.equal(L.nudge("ArrowUp", false, "left", now), null);
  assert.equal(L.nudge("a", false, "left", now), null);
});

console.log(`test-layout-widths: ${passed} checks passed`);
