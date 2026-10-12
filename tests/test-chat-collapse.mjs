#!/usr/bin/env node
// Regression script for src/app/chat-collapse.js — which tool cards and
// thinking blocks the chat shows collapsed (#44), per kind (#43).
// Run: node tests/test-chat-collapse.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src  = readFileSync(join(root, "src/app/chat-collapse.js"), "utf8");
const win  = {};
// eslint-disable-next-line no-new-func
new Function("window", src)(win);
const C = win.OMP_COLLAPSE;

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

// A finished bash card with `n` output lines.
const bash = (id, n, extra = {}) => ({
  _id: id, kind: "tool", tool: "bash", status: "ok",
  output: Array.from({ length: n }, (_, i) => ({ line: `l${i}`, color: "fg" })), ...extra,
});
const LONG  = C.SHORT_LINES + 1;
const SHORT = C.SHORT_LINES;          // has a toggle, never collapses on its own
const TINY  = C.PREVIEW_LINES;        // nothing to hide
const longRun = (n, start = 1) => Array.from({ length: n }, (_, i) => bash(start + i, LONG));

check("everything starts expanded", () => {
  const msgs = longRun(3);
  assert.deepEqual(C.collapseStates(msgs, { after: 3 }), { 1: false, 2: false, 3: false });
});

check("a long block collapses once `after` newer blocks follow it", () => {
  const msgs = longRun(4);
  assert.deepEqual(C.collapseStates(msgs, { after: 3 }), { 1: true, 2: false, 3: false, 4: false });
  const more = longRun(6);
  assert.deepEqual(C.collapseStates(more, { after: 3 }), { 1: true, 2: true, 3: true, 4: false, 5: false, 6: false });
});

check("a short block keeps its toggle but never collapses on its own; it still ages older ones", () => {
  const msgs = [bash(1, SHORT), bash(2, LONG), bash(3, SHORT), bash(4, SHORT)];
  assert.deepEqual(C.collapseStates(msgs, { after: 2 }), { 1: false, 2: true, 3: false, 4: false });
});

check("a block with nothing to hide gets no toggle and does not age older ones", () => {
  const msgs = [bash(1, LONG), bash(2, TINY), bash(3, TINY), { _id: 4, kind: "tool", tool: "read", status: "ok" }];
  assert.deepEqual(C.collapseStates(msgs, { after: 1 }), { 1: false });
});

check("the user's toggle wins in both directions", () => {
  const msgs = [...longRun(4), bash(5, SHORT)];
  const states = C.collapseStates(msgs, { after: 2, overrides: { 1: false, 5: true } });
  assert.equal(states[1], false, "kept open although old");
  assert.equal(states[2], true, "untouched neighbour still auto-collapses");
  assert.equal(states[5], true, "collapsed although short and newest");
});

check("a running tool or a streaming reply never collapses on its own", () => {
  const msgs = [
    bash(1, LONG, { status: "running" }),
    { _id: 2, kind: "assistant", streaming: true, thought: "x".repeat(2000) },
    ...longRun(3, 3),
  ];
  const states = C.collapseStates(msgs, { after: 1 });
  assert.equal(states[1], false);
  assert.equal(states[2], false);
  assert.equal(states[3], true, "finished blocks around them still collapse");
});

check("a streaming reply whose thinking grows in place gets its toggle, and ages older blocks", () => {
  // live.js grows `streamingBubble` in place, and that object can sit in
  // the transcript (after message_start, and after a get_messages merge).
  const streaming = { _id: 2, kind: "assistant", streaming: true, thought: "short" };
  const msgs = [bash(1, LONG), streaming];
  assert.deepEqual(C.collapseStates(msgs, { after: 1 }), { 1: false });
  streaming.thought = "word ".repeat(400);
  assert.deepEqual(C.collapseStates(msgs, { after: 1 }), { 1: true, 2: false });
});

// A finished reply whose thinking runs ~20 wrapped lines.
const think = (id, extra = {}) => ({ _id: id, kind: "assistant", thought: "word ".repeat(400), ...extra });
const edit  = (id, extra = {}) => ({ _id: id, kind: "tool", tool: "edit", status: "ok", diff: new Array(10).fill({}), ...extra });

check("open: nothing collapses unless the user collapses it", () => {
  const msgs = longRun(6);
  const states = C.collapseStates(msgs, { thinking: "open", tool: "open", after: 1, overrides: { 2: true } });
  assert.deepEqual(states, { 1: false, 2: true, 3: false, 4: false, 5: false, 6: false });
});

check("folded tool output: every card starts collapsed, short and newest ones too", () => {
  const msgs = [bash(1, LONG), bash(2, SHORT), bash(3, LONG)];
  assert.deepEqual(C.collapseStates(msgs, { tool: "folded", after: 30 }), { 1: true, 2: true, 3: true });
});

check("folded tool output keeps a running card, an edit's diff and a failed call open", () => {
  const msgs = [edit(1), bash(2, LONG, { isError: true }), bash(3, LONG, { status: "running" }), bash(4, LONG)];
  assert.deepEqual(C.collapseStates(msgs, { tool: "folded", after: 1 }), { 1: false, 2: false, 3: false, 4: true });
});

check("auto collapses an old edit and an old failed call like any other card", () => {
  const msgs = [edit(1), bash(2, LONG, { isError: true }), ...longRun(2, 3)];
  assert.deepEqual(C.collapseStates(msgs, { after: 2 }), { 1: true, 2: true, 3: false, 4: false });
});

check("folded thinking starts collapsed, even while the reply streams", () => {
  const msgs = [think(1), think(2, { streaming: true })];
  assert.deepEqual(C.collapseStates(msgs, { thinking: "folded", after: 30 }), { 1: true, 2: true });
});

check("each kind follows its own mode", () => {
  const msgs = [think(1), bash(2, LONG), think(3), bash(4, LONG)];
  assert.deepEqual(C.collapseStates(msgs, { thinking: "open", tool: "folded", after: 1 }),
    { 1: false, 2: true, 3: false, 4: true });
  assert.deepEqual(C.collapseStates(msgs, { thinking: "folded", tool: "open", after: 1 }),
    { 1: true, 2: false, 3: true, 4: false });
});

check("in auto, blocks of the other kind still age a block, whatever their own mode", () => {
  const msgs = [bash(1, LONG), think(2), think(3)];
  assert.deepEqual(C.collapseStates(msgs, { thinking: "open", tool: "auto", after: 2 }), { 1: true, 2: false, 3: false });
});

check("the user's toggle beats folded and open", () => {
  const msgs = [bash(1, LONG), think(2)];
  assert.deepEqual(C.collapseStates(msgs, { thinking: "open", tool: "folded", overrides: { 1: false, 2: true } }),
    { 1: false, 2: true });
});

check("while reading history, blocks after the horizon do not collapse older ones", () => {
  const msgs = longRun(6);
  // Scrolled away when message 3 was the latest: 1 had 2 newer blocks then.
  assert.deepEqual(C.collapseStates(msgs, { after: 2, horizonId: 3 }),
    { 1: true, 2: false, 3: false, 4: false, 5: false, 6: false });
  // Back at the bottom: the backlog collapses.
  assert.deepEqual(C.collapseStates(msgs, { after: 2, horizonId: null }),
    { 1: true, 2: true, 3: true, 4: true, 5: false, 6: false });
});

check("a horizon that is no longer in the transcript freezes nothing", () => {
  const msgs = longRun(3);
  assert.deepEqual(C.collapseStates(msgs, { after: 1, horizonId: 99 }), { 1: true, 2: true, 3: false });
});

check("messages the chat does not render neither collapse nor age others", () => {
  const msgs = [bash(1, LONG), bash(2, LONG, { superseded: true }), bash(3, LONG)];
  const states = C.collapseStates(msgs, { after: 2, hidden: m => !!m.superseded });
  assert.deepEqual(states, { 1: false, 3: false });
});

check("thinking counts as a block, measured as wrapped prose", () => {
  const thought = "word ".repeat(400);        // 2000 chars, ~20 wrapped lines
  const msgs = [{ _id: 1, kind: "assistant", thought }, bash(2, LONG)];
  assert.deepEqual(C.collapseStates(msgs, { after: 1 }), { 1: true, 2: false });
  assert.equal(C.bodyLines({ kind: "assistant", thought: "short thought" }), 1);
  assert.equal(C.bodyLines({ kind: "assistant", thought: null }), 0);
});

check("body size per tool: diff lines plus chrome, eval code and output, subagent rows", () => {
  assert.equal(C.bodyLines({ kind: "tool", tool: "edit", diff: new Array(10).fill({}) }), 12);
  assert.equal(C.bodyLines({ kind: "tool", tool: "edit" }), 1);
  assert.equal(C.bodyLines({ kind: "tool", tool: "eval", cells: [
    { code: "a\nb\n", output: "1\n2\n3" },
    { code: "", output: "" },
  ] }), (1 + 2 + 3) + 1);
  assert.equal(C.bodyLines({ kind: "tool", tool: "task", subagents: [{}, {}, {}, {}] }), 4);
  assert.equal(C.bodyLines({ kind: "tool", tool: "goal" }), 0);
  assert.equal(C.bodyLines({ kind: "user", text: "x".repeat(5000) }), 0);
});

check("a bash card with a few very long lines counts its wrapped lines, so it can collapse", () => {
  const dump = { _id: 1, kind: "tool", tool: "bash", status: "ok", output: [{ line: "x".repeat(1000), color: "fg" }] };
  assert.equal(C.bodyLines(dump), 10);
  assert.deepEqual(C.collapseStates([dump, bash(2, LONG)], { after: 1 }), { 1: true, 2: false });
});

check("clampAfter keeps the tweak in range", () => {
  assert.equal(C.clampAfter(0), C.MIN_AFTER);
  assert.equal(C.clampAfter(1000), C.MAX_AFTER);
  assert.equal(C.clampAfter("7"), 7);
  assert.equal(C.clampAfter(undefined), C.DEFAULT_AFTER);
});

check("a collapsed bash card shows its last non-blank lines, notices included", () => {
  const out = ["18", "19", "20", "", "", "Wall time: 0.04 seconds", ""].map(line => ({ line, color: "fg-3" }));
  assert.deepEqual(C.bashPreview(out).map(l => l.line), ["19", "20", "Wall time: 0.04 seconds"]);
  assert.deepEqual(C.bashPreview([{ line: "", color: "fg" }]), []);
});

check("foldModes: a picked mode wins; unset or unknown falls back to the old auto-collapse switch", () => {
  assert.deepEqual(C.foldModes({}), { thinking: "auto", tool: "auto" });
  assert.deepEqual(C.foldModes(undefined), { thinking: "auto", tool: "auto" });
  assert.deepEqual(C.foldModes({ foldThinking: null, foldTools: null, autoCollapse: false }), { thinking: "open", tool: "open" });
  assert.deepEqual(C.foldModes({ foldThinking: "folded", foldTools: "bogus", autoCollapse: false }), { thinking: "folded", tool: "open" });
  assert.deepEqual(C.foldModes({ foldTools: "folded", autoCollapse: true }), { thinking: "auto", tool: "folded" });
});

check("overridesForAll sets every block with a toggle, skips tiny and hidden ones; later blocks follow the modes", () => {
  const msgs = [bash(1, LONG), bash(2, TINY), think(3), bash(4, LONG, { superseded: true }), bash(5, SHORT)];
  const hidden = m => !!m.superseded;
  const all = C.overridesForAll(msgs, true, { hidden });
  assert.deepEqual(all, { 1: true, 3: true, 5: true });
  assert.deepEqual(C.overridesForAll(msgs, false, { hidden }), { 1: false, 3: false, 5: false });
  const later = [...msgs, bash(6, LONG)];
  assert.deepEqual(C.collapseStates(later, { after: 30, overrides: all, hidden }), { 1: true, 3: true, 5: true, 6: false });
});

check("Expand all covers a running card still too short for a toggle: it stays expanded once it grows", () => {
  const running = bash(1, 0, { status: "running" });
  const all = C.overridesForAll([running], false);
  assert.deepEqual(all, { 1: false });
  const finished = bash(1, LONG);
  assert.deepEqual(C.collapseStates([finished], { tool: "folded", overrides: all }), { 1: false });
  // Without the toggle, folded would have collapsed it.
  assert.deepEqual(C.collapseStates([finished], { tool: "folded" }), { 1: true });
});

console.log(`chat-collapse: ${passed} checks passed`);
