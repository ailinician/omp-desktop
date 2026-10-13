#!/usr/bin/env node
// Regression script for src/app/terminal.js — the terminal panel's pure
// parts: per-tab view state (open / created / Rust's status / start error),
// which chords the global keymap still gets while xterm has focus, the
// shell label and the panel height clamp.
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
const E = (over) => ({ open: false, created: false, status: null, error: null, ...over });

/** A `TerminalStatus` as Rust sends it. */
const St = (instance, state = "running", exitCode = null, shell = "/bin/zsh") =>
  ({ instance, shell, state, exitCode });

const opened = (sid = "a") => T.openPanel(T.EMPTY, sid);

check("unknown session reads as closed, with no shell", () => {
  assert.deepEqual(T.entryOf(T.EMPTY, "a"), E({}));
  assert.deepEqual(T.entryOf(undefined, "a"), E({}));
});

check("openPanel opens one tab and leaves the others alone", () => {
  const s = T.openPanel(opened("a"), "b");
  assert.equal(T.entryOf(s, "a").open, true);
  assert.equal(T.entryOf(s, "b").open, true);
  assert.equal(T.entryOf(s, "c").open, false);
  assert.equal(T.entryOf(T.EMPTY, "a").open, false, "EMPTY is never mutated");
});

check("closePanel hides but keeps the shell's status", () => {
  const s = T.closePanel(T.applyStatus(opened(), "a", St(1)), "a");
  assert.deepEqual(T.entryOf(s, "a"), E({ created: true, status: St(1) }));
});

check("a panel that was opened stays created when hidden, even before any spawn", () => {
  // The view keeps `created` panels mounted: a hide during the first (still
  // pending or failed) spawn must not drop the xterm.
  const s = T.closePanel(opened(), "a");
  assert.equal(T.entryOf(s, "a").created, true);
  assert.equal(T.entryOf(s, "a").open, false);
  assert.equal(T.entryOf(s, "a").status, null);
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

// ---- Rust status / exit ---------------------------------------------------

check("applyStatus records Rust's status", () => {
  const s = T.applyStatus(opened(), "a", St(3));
  assert.deepEqual(T.entryOf(s, "a"), E({ open: true, created: true, status: St(3) }));
});

check("applyStatus ignores null, a tab with no entry, and an older instance", () => {
  const s = T.applyStatus(opened(), "a", St(5));
  assert.equal(T.applyStatus(s, "a", null), s, "null = no news");
  assert.equal(T.applyStatus(T.EMPTY, "gone", St(1)), T.EMPTY, "closed tab");
  assert.equal(T.applyStatus(s, "a", St(4, "running")), s, "stale instance");
  assert.equal(T.applyStatus(s, "a", St(4, "exited", 0)), s, "stale exit report");
});

check("a late status of the same instance never hides its exit", () => {
  // terminal_open's reply (running) can arrive after the exit event.
  let s = T.applyStatus(opened(), "a", St(2, "starting"));
  s = T.applyExit(s, "a", 2, 1);
  assert.equal(T.applyStatus(s, "a", St(2, "running")), s);
  assert.equal(T.applyStatus(s, "a", St(2, "starting")), s);
  assert.deepEqual(T.entryOf(s, "a").status, St(2, "exited", 1));
});

check("applyStatus moves forward within an instance and to a newer one", () => {
  let s = T.applyStatus(opened(), "a", St(2, "starting"));
  s = T.applyStatus(s, "a", St(2, "running"));
  assert.equal(T.entryOf(s, "a").status.state, "running");
  s = T.applyStatus(s, "a", St(2, "exited", 0));
  s = T.applyStatus(s, "a", St(7, "running", null, "/bin/bash"));
  assert.deepEqual(T.entryOf(s, "a").status, St(7, "running", null, "/bin/bash"));
});

check("applyExit marks only the current instance exited, with its code", () => {
  const s = T.applyStatus(opened(), "a", St(4));
  assert.equal(T.applyExit(s, "a", 3, 0), s, "older instance ignored");
  assert.equal(T.applyExit(s, "a", 5, 0), s, "unknown newer instance ignored");
  const ex = T.applyExit(s, "a", 4, 130);
  assert.deepEqual(T.entryOf(ex, "a").status, St(4, "exited", 130));
});

check("applyExit without a shown shell or entry is ignored", () => {
  const s = opened();
  assert.equal(T.applyExit(s, "a", 1, 0), s, "no status yet");
  assert.equal(T.applyExit(T.EMPTY, "gone", 1, 0), T.EMPTY, "no entry");
});

check("an exit killed by a signal (or a bad code) has exitCode null", () => {
  const s = T.applyStatus(opened(), "a", St(1));
  assert.equal(T.entryOf(T.applyExit(s, "a", 1, null), "a").status.exitCode, null);
  assert.equal(T.entryOf(T.applyExit(s, "a", 1, "9"), "a").status.exitCode, null);
});

check("a repeated exit is a no-op", () => {
  const s = T.applyExit(T.applyStatus(opened(), "a", St(1)), "a", 1, 0);
  assert.equal(T.applyExit(s, "a", 1, 0), s);
});

check("spawnFailed records the error; a later status clears it", () => {
  let s = T.spawnFailed(opened(), "a", "no such file");
  assert.equal(T.entryOf(s, "a").error, "no such file");
  assert.equal(T.spawnFailed(T.EMPTY, "gone", "x"), T.EMPTY, "closed tab");
  s = T.applyStatus(s, "a", St(1));
  assert.equal(T.entryOf(s, "a").error, null);
});

// ---- needsSpawn / isExited ------------------------------------------------

check("needsSpawn: only a shown, created panel whose tab never had a shell", () => {
  assert.equal(T.needsSpawn(T.entryOf(T.EMPTY, "a")), false, "never opened");
  const s = opened();
  assert.equal(T.needsSpawn(T.entryOf(s, "a")), true, "opened, no shell");
  assert.equal(T.needsSpawn(T.entryOf(T.closePanel(s, "a"), "a")), false, "hidden");
  const live = T.applyStatus(s, "a", St(1, "starting"));
  assert.equal(T.needsSpawn(T.entryOf(live, "a")), false, "starting");
  const exited = T.applyExit(T.applyStatus(s, "a", St(1)), "a", 1, 0);
  assert.equal(T.needsSpawn(T.entryOf(exited, "a")), false, "exited: no auto respawn");
  const failed = T.spawnFailed(s, "a", "boom");
  assert.equal(T.needsSpawn(T.entryOf(failed, "a")), false, "failed start: no retry loop");
});

check("isExited: an exited shell or a failed start; not starting/running/none", () => {
  const s = opened();
  assert.equal(T.isExited(T.entryOf(s, "a")), false, "no shell yet");
  assert.equal(T.isExited(T.entryOf(T.applyStatus(s, "a", St(1, "starting")), "a")), false);
  assert.equal(T.isExited(T.entryOf(T.applyStatus(s, "a", St(1)), "a")), false);
  assert.equal(T.isExited(T.entryOf(T.applyStatus(s, "a", St(1, "exited", 0)), "a")), true);
  assert.equal(T.isExited(T.entryOf(T.spawnFailed(s, "a", "boom"), "a")), true);
});

check("respawn: the next instance's status ends the exited state", () => {
  let s = T.applyExit(T.applyStatus(opened(), "a", St(1)), "a", 1, 2);
  s = T.applyStatus(s, "a", St(2));
  assert.equal(T.isExited(T.entryOf(s, "a")), false);
  assert.equal(T.applyExit(s, "a", 1, 0), s, "the old shell's late exit is stale");
});

check("closing then reopening an exited panel keeps the exit state", () => {
  let s = T.applyExit(T.applyStatus(opened(), "a", St(1)), "a", 1, 0);
  s = T.openPanel(T.closePanel(s, "a"), "a");
  assert.equal(T.isExited(T.entryOf(s, "a")), true);
  assert.equal(T.needsSpawn(T.entryOf(s, "a")), false);
});

check("no-op transitions return the same object", () => {
  const s = T.applyStatus(opened(), "a", St(1));
  assert.equal(T.openPanel(s, "a"), s);
  assert.equal(T.applyStatus(s, "a", St(1)), s, "same status again");
  assert.equal(T.closePanel(T.EMPTY, "a"), T.EMPTY);
  assert.equal(T.prune(s, ["a", "b"]), s);
});

check("prune drops entries of sessions that no longer exist", () => {
  let s = T.applyStatus(T.openPanel(opened("a"), "b"), "b", St(1));
  s = T.prune(s, ["a"]);
  assert.equal(T.entryOf(s, "a").open, true);
  assert.deepEqual(T.entryOf(s, "b"), E({}));
  assert.deepEqual(Object.keys(s), ["a"]);
  assert.equal(T.applyExit(s, "b", 1, 0), s, "a pruned tab's late exit is ignored");
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
