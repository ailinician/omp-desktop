#!/usr/bin/env node
// Regression script for src/app/terminal.js — the terminal panel's pure
// parts: per-tab panel state (open / spawned / exited), which chords the
// global keymap still gets while xterm has focus, the base64 decoder for
// pty output frames, the shell label and the panel height clamp.
// Run: node tests/test-terminal.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// keymap.js must load first — terminal.js calls window.OMP_KEYMAP.chordFromEvent
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/keymap.js"), "utf8"))(win);
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/terminal.js"), "utf8"))(win);
const T = win.OMP_TERMINAL;

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

// ---- panel state ----------------------------------------------------------

/** An entry as the reducer shapes it, with `over` applied. */
const E = (over) => ({
  open: false, created: false, spawned: false, exited: false, exitCode: null, exitGen: null, ...over,
});

check("unknown session reads as closed, unspawned", () => {
  assert.deepEqual(T.entryOf(T.EMPTY, "a"), E({}));
  assert.deepEqual(T.entryOf(undefined, "a"), E({}));
});

check("openPanel opens one tab and leaves the others alone", () => {
  const s = T.openPanel(T.openPanel(T.EMPTY, "a"), "b");
  assert.equal(T.entryOf(s, "a").open, true);
  assert.equal(T.entryOf(s, "b").open, true);
  assert.equal(T.entryOf(s, "c").open, false);
  assert.equal(T.entryOf(T.EMPTY, "a").open, false, "EMPTY is never mutated");
});

check("closePanel hides but keeps the shell (spawned survives)", () => {
  const s = T.closePanel(T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1), "a");
  assert.deepEqual(T.entryOf(s, "a"), E({ created: true, spawned: true }));
});

check("a panel that was opened stays created when hidden, even before any spawn", () => {
  // The view keeps `created` panels mounted: a hide during the first (still
  // pending or failed) spawn must not drop the xterm.
  const s = T.closePanel(T.openPanel(T.EMPTY, "a"), "a");
  assert.equal(T.entryOf(s, "a").created, true);
  assert.equal(T.entryOf(s, "a").open, false);
  assert.equal(T.entryOf(s, "a").spawned, false);
  assert.equal(T.entryOf(T.EMPTY, "a").created, false, "never opened: no instance");
});

check("togglePanel flips open", () => {
  const s1 = T.togglePanel(T.EMPTY, "a");
  assert.equal(T.entryOf(s1, "a").open, true);
  assert.equal(T.entryOf(s1, "a").created, true);
  const s2 = T.togglePanel(s1, "a");
  assert.equal(T.entryOf(s2, "a").open, false);
  assert.equal(T.entryOf(s2, "a").created, true);
});

check("markSpawned records the spawn", () => {
  const s = T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1);
  assert.deepEqual(T.entryOf(s, "a"), E({ open: true, created: true, spawned: true }));
});

check("markExited keeps spawned (no auto respawn) and stores the code", () => {
  const s = T.markExited(T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1), "a", 130, 1);
  assert.deepEqual(
    T.entryOf(s, "a"),
    E({ open: true, created: true, spawned: true, exited: true, exitCode: 130, exitGen: 1 }),
  );
});

check("markExited on an exit with no code keeps exitCode null", () => {
  const s = T.markExited(T.markSpawned(T.EMPTY, "a", 1), "a", undefined, 1);
  assert.equal(T.entryOf(s, "a").exited, true);
  assert.equal(T.entryOf(s, "a").exitCode, null);
});

check("markExited for a session with no entry is ignored", () => {
  assert.equal(T.markExited(T.EMPTY, "gone", 0, 1), T.EMPTY);
});

check("respawn: markSpawned of the next generation clears exited and the code", () => {
  let s = T.markExited(T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1), "a", 1, 1);
  s = T.markSpawned(s, "a", 2);
  assert.deepEqual(T.entryOf(s, "a"), E({ open: true, created: true, spawned: true }));
});

check("an exit that beats terminal_open's reply survives that generation's markSpawned", () => {
  // Spawn generation 1 starts; the shell dies at once and its exit event
  // arrives before `terminal_open` resolves; only then markSpawned runs.
  let s = T.openPanel(T.EMPTY, "a");
  s = T.markExited(s, "a", 0, 1);
  s = T.markSpawned(s, "a", 1);
  assert.deepEqual(
    T.entryOf(s, "a"),
    E({ open: true, created: true, spawned: true, exited: true, exitCode: 0, exitGen: 1 }),
  );
});

check("an earlier generation's exit does not survive a later generation's markSpawned", () => {
  // gen 1 exited normally; the respawn (gen 2) reports its spawn: exited cleared.
  let s = T.markExited(T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1), "a", 7, 1);
  s = T.markSpawned(s, "a", 2);
  assert.equal(T.entryOf(s, "a").exited, false);
  assert.equal(T.entryOf(s, "a").exitCode, null);
  // And a gen-2 exit that races its own markSpawned survives again.
  s = T.markExited(s, "a", 9, 2);
  s = T.markSpawned(s, "a", 2);
  assert.equal(T.entryOf(s, "a").exited, true);
  assert.equal(T.entryOf(s, "a").exitCode, 9);
});

check("closing then reopening an exited panel keeps the exit state", () => {
  let s = T.markExited(T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1), "a", 0, 1);
  s = T.openPanel(T.closePanel(s, "a"), "a");
  assert.deepEqual(
    T.entryOf(s, "a"),
    E({ open: true, created: true, spawned: true, exited: true, exitCode: 0, exitGen: 1 }),
  );
});

check("no-op transitions return the same object", () => {
  const s = T.markSpawned(T.openPanel(T.EMPTY, "a"), "a", 1);
  assert.equal(T.openPanel(s, "a"), s);
  assert.equal(T.markSpawned(s, "a", 1), s);
  assert.equal(T.closePanel(T.EMPTY, "a"), T.EMPTY);
  assert.equal(T.prune(s, ["a", "b"]), s);
});

check("prune drops entries of sessions that no longer exist", () => {
  let s = T.markSpawned(T.openPanel(T.openPanel(T.EMPTY, "a"), "b"), "b", 1);
  s = T.prune(s, ["a"]);
  assert.equal(T.entryOf(s, "a").open, true);
  assert.deepEqual(T.entryOf(s, "b"), E({}));
  assert.deepEqual(Object.keys(s), ["a"]);
});

// ---- key bypass -----------------------------------------------------------

const ev = (key, mods = "") => ({
  key,
  ctrlKey: mods.includes("c"),
  shiftKey: mods.includes("s"),
  altKey: mods.includes("a"),
  metaKey: mods.includes("m"),
  isComposing: false,
});

check("super+t/w/k/h/b// bypass the terminal", () => {
  for (const k of ["t", "w", "k", "h", "b", "/", "`"]) {
    assert.equal(T.keymapShouldBypass(ev(k, "m")), true, `super+${k}`);
  }
  assert.equal(T.keymapShouldBypass(ev("T", "m")), true, "caps lock → super+t");
});

check("ctrl+` and ctrl+tab chords bypass", () => {
  assert.equal(T.keymapShouldBypass(ev("`", "c")), true);
  assert.equal(T.keymapShouldBypass(ev("Tab", "c")), true);
  assert.equal(T.keymapShouldBypass(ev("Tab", "cs")), true);
});

check("shifted variants of bypass keys still bypass (chordFromEvent normalisation)", () => {
  // German Cmd+Shift+7 → key "/" with shiftKey — shift cleared for non-alpha
  assert.equal(T.keymapShouldBypass({ key: "/", metaKey: true, shiftKey: true, ctrlKey: false, altKey: false, isComposing: false }), true);
  // Shifted backtick → "~" — shift cleared, "super+~" in BYPASS
  assert.equal(T.keymapShouldBypass(ev("~", "m")), true, "super+~ (shifted backtick)");
});

check("shifted letter does not bypass", () => {
  assert.equal(T.keymapShouldBypass({ key: "T", metaKey: true, shiftKey: true, ctrlKey: false, altKey: false, isComposing: false }), false, "super+shift+t");
});

check("keys the shell owns do not bypass", () => {
  assert.equal(T.keymapShouldBypass(ev("Escape")), false);
  assert.equal(T.keymapShouldBypass(ev("c", "c")), false, "ctrl+c");
  assert.equal(T.keymapShouldBypass(ev("w", "c")), false, "ctrl+w kills a word in the shell");
  assert.equal(T.keymapShouldBypass(ev("t", "c")), false, "ctrl+t");
  assert.equal(T.keymapShouldBypass(ev("Tab")), false, "plain Tab completes");
  assert.equal(T.keymapShouldBypass(ev("Tab", "s")), false, "shift+Tab");
  assert.equal(T.keymapShouldBypass(ev("a")), false);
  assert.equal(T.keymapShouldBypass(ev("t")), false);
  assert.equal(T.keymapShouldBypass(ev("Enter")), false);
});

check("copy/paste and extra modifiers do not bypass", () => {
  assert.equal(T.keymapShouldBypass(ev("c", "m")), false, "super+c copies");
  assert.equal(T.keymapShouldBypass(ev("v", "m")), false, "super+v pastes");
  assert.equal(T.keymapShouldBypass(ev("t", "ms")), false, "super+shift+t");
  assert.equal(T.keymapShouldBypass(ev("t", "ma")), false, "super+alt+t");
  assert.equal(T.keymapShouldBypass(ev("`", "ca")), false);
  assert.equal(T.keymapShouldBypass(ev("Tab", "ca")), false);
});

check("a composing event never bypasses", () => {
  assert.equal(T.keymapShouldBypass({ ...ev("t", "m"), isComposing: true }), false);
});

// ---- base64 ---------------------------------------------------------------

const b64 = (bytes) => Buffer.from(bytes).toString("base64");

check("decodeB64 round-trips every length mod 3 (padding)", () => {
  for (const n of [0, 1, 2, 3, 4, 5, 6, 255]) {
    const bytes = Uint8Array.from({ length: n }, (_, i) => (i * 37 + 11) & 0xff);
    const out = T.decodeB64(b64(bytes));
    assert.ok(out instanceof Uint8Array);
    assert.deepEqual([...out], [...bytes], `len ${n}`);
  }
});

check("decodeB64 keeps multibyte UTF-8 and binary bytes intact", () => {
  const text = "größe ✓ 日本語 🚀 \x1b[31mred\x1b[0m";
  const out = T.decodeB64(b64(Buffer.from(text, "utf8")));
  assert.equal(Buffer.from(out).toString("utf8"), text);
  const all = Uint8Array.from({ length: 256 }, (_, i) => i);
  assert.deepEqual([...T.decodeB64(b64(all))], [...all]);
});

check("decodeB64 rejects garbage instead of returning wrong bytes", () => {
  assert.throws(() => T.decodeB64("ab*d"));
  assert.throws(() => T.decodeB64("abcde"), "length 1 mod 4 is impossible");
  assert.throws(() => T.decodeB64(null));
});

// ---- shellLabel / clampHeight ---------------------------------------------

check("shellLabel is the basename without .exe", () => {
  assert.equal(T.shellLabel("/bin/zsh"), "zsh");
  assert.equal(T.shellLabel("/usr/local/bin/fish/"), "fish");
  assert.equal(T.shellLabel("C:\\Windows\\System32\\cmd.exe"), "cmd");
  assert.equal(T.shellLabel("bash"), "bash");
  assert.equal(T.shellLabel(""), "shell");
  assert.equal(T.shellLabel(undefined), "shell");
});

check("clampHeight stays within [120, 70% of column]", () => {
  assert.equal(T.clampHeight(240, 1000), 240);
  assert.equal(T.clampHeight(50, 1000), 120, "raised to MIN_HEIGHT");
  assert.equal(T.clampHeight(900, 1000), 700);
  assert.equal(T.clampHeight(240.6, 1000), 241);
  assert.equal(T.clampHeight(500, 100), 70, "max-wins: 0.7×100=70, column share wins over old 120 min");
  assert.equal(T.clampHeight(NaN, 1000), T.DEFAULT_HEIGHT);
  assert.equal(T.clampHeight(undefined, 1000), T.DEFAULT_HEIGHT);
  assert.equal(T.DEFAULT_HEIGHT, 240);
});

check("clampHeight with non-finite px substitutes DEFAULT_HEIGHT then clamps", () => {
  const h = T.clampHeight(undefined, 300);
  assert.ok(h <= 210 && h >= 120, `undefined→DEFAULT_HEIGHT=240 clamped to [0,210]: got ${h}`);
  assert.equal(T.clampHeight(NaN, 1000), 240);
});

console.log(`terminal: ${passed} checks passed`);
