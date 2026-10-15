#!/usr/bin/env node
// Regression script for src/app/terminal.js — the terminal panel's pure
// parts: per-tab view state (open / created / Rust's status / start error),
// when the shell is live (resizes) and Enter may restart it, the copy /
// paste chords, the shell label and the panel height clamp. Which chords
// the keymap still gets while xterm has focus is app/keymap.js's
// `guestShouldBypass` (test-keymap.mjs).
// Run: node tests/test-terminal.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// keymap.js must load first — terminal.js reads window.OMP_KEYMAP (chords,
// platform detection) — and project-nav.js + path-setting.js give
// shellLabel / shellMenu their names and rows.
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/keymap.js"), "utf8"))(win);
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/project-nav.js"), "utf8"))(win);
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/path-setting.js"), "utf8"))(win);
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

// ---- shellLive / canRestart ------------------------------------------------

check("shellLive: a starting or running shell; not none, exited or failed", () => {
  const s = opened();
  const at = (st) => T.entryOf(st, "a");
  assert.equal(T.shellLive(at(s)), false, "no shell yet: the first fit sends no resize");
  assert.equal(T.shellLive(at(T.applyStatus(s, "a", St(1, "starting")))), true);
  assert.equal(T.shellLive(at(T.applyStatus(s, "a", St(1)))), true);
  assert.equal(T.shellLive(at(T.applyStatus(s, "a", St(1, "exited", 0)))), false);
  assert.equal(T.shellLive(at(T.spawnFailed(s, "a", "boom"))), false);
});

check("shellLive: an open reply newer than the rendered status counts as live", () => {
  const exited = T.entryOf(T.applyExit(T.applyStatus(opened(), "a", St(1)), "a", 1, 0), "a");
  assert.equal(T.shellLive(exited, 1), false, "the open that started the exited shell");
  assert.equal(T.shellLive(exited, 2), true, "respawn replied, not rendered yet");
  const failed = T.entryOf(T.spawnFailed(opened(), "a", "boom"), "a");
  assert.equal(T.shellLive(failed, 0), false);
  assert.equal(T.shellLive(failed, 1), true, "a retry replied, not rendered yet");
});

check("canRestart: Enter replaces an exited or failed shell exactly once", () => {
  const s = T.applyStatus(opened(), "a", St(1));
  assert.equal(T.canRestart(T.entryOf(s, "a"), 1), false, "running");
  assert.equal(T.canRestart(T.entryOf(opened(), "a"), 0), false, "no shell yet: the view spawns it");
  const exited = T.applyExit(s, "a", 1, 0);
  assert.equal(T.canRestart(T.entryOf(exited, "a"), 1), true, "exited");
  assert.equal(T.canRestart(T.entryOf(exited, "a"), 2), false, "second Enter before the respawn rendered");
  const respawned = T.applyStatus(exited, "a", St(2));
  assert.equal(T.canRestart(T.entryOf(respawned, "a"), 2), false, "rendered: running");
  assert.equal(T.canRestart(T.entryOf(T.applyExit(respawned, "a", 2, 1), "a"), 2), true, "the new shell exited");
  const failed = T.spawnFailed(opened(), "a", "boom");
  assert.equal(T.canRestart(T.entryOf(failed, "a"), 0), true, "failed start: retry");
  assert.equal(T.canRestart(T.entryOf(failed, "a"), 1), false, "retry replied, not rendered yet");
});

check("no-op transitions return the same object", () => {
  const s = T.applyStatus(opened(), "a", St(1));
  assert.equal(T.openPanel(s, "a"), s);
  assert.equal(T.applyStatus(s, "a", St(1)), s, "same status again");
  assert.equal(T.closePanel(T.EMPTY, "a"), T.EMPTY);
});

// ---- clipboard chords -----------------------------------------------------

const ev = (key, mods = "") => ({
  key,
  ctrlKey: mods.includes("c"),
  shiftKey: mods.includes("s"),
  altKey: mods.includes("a"),
  metaKey: mods.includes("m"),
  isComposing: false,
});

// Explicit platform per assertion: the result must not depend on the host.
const LINUX = false;
const MAC = true;

check("clipboardAction: ctrl+shift+c / v off macOS", () => {
  assert.equal(T.clipboardAction(ev("C", "cs"), LINUX), "copy");
  assert.equal(T.clipboardAction(ev("V", "cs"), LINUX), "paste");
  assert.equal(T.clipboardAction(ev("c", "c"), LINUX), null, "ctrl+c is the shell's interrupt");
  assert.equal(T.clipboardAction(ev("v", "c"), LINUX), null, "ctrl+v is the shell's");
  assert.equal(T.clipboardAction(ev("C", "csa"), LINUX), null, "ctrl+shift+alt+c");
  assert.equal(T.clipboardAction({ ...ev("V", "cs"), isComposing: true }, LINUX), null, "composing");
});

check("clipboardAction: none on macOS (native ⌘C / ⌘V)", () => {
  assert.equal(T.clipboardAction(ev("C", "cs"), MAC), null);
  assert.equal(T.clipboardAction(ev("c", "m"), MAC), null);
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

// ---- shellMenu --------------------------------------------------------------

const SHELLS = [
  { name: "cmd", path: "C:\\WINDOWS\\System32\\cmd.exe" },
  { name: "pwsh", path: "C:\\Program Files\\PowerShell\\7\\pwsh.exe" },
];
const menu = (shell) => T.shellMenu({ shell, defaultShell: "C:\\WINDOWS\\system32\\cmd.exe", shells: SHELLS });
const checkedOf = (m) => m.rows.filter((r) => r.checked).map((r) => r.name);

check("shellMenu: an empty setting checks default and names the default shell", () => {
  const m = menu("");
  assert.equal(m.label, "cmd");
  assert.deepEqual(m.rows.map((r) => [r.name, r.hint, r.value]), [
    ["default", "cmd", ""], ["cmd", "", SHELLS[0].path], ["pwsh", "", SHELLS[1].path],
  ]);
  assert.deepEqual(checkedOf(m), ["default"]);
});

check("shellMenu: an installed shell is checked by its path, not as default", () => {
  const m = menu(SHELLS[1].path);
  assert.equal(m.label, "pwsh");
  assert.deepEqual(checkedOf(m), ["pwsh"]);
  assert.equal(m.rows.length, 3, "no extra row");
});

check("shellMenu: a Windows path in another case or with / checks the listed row", () => {
  for (const stored of ["c:\\Windows\\system32\\CMD.EXE", "C:/Windows/System32/cmd.exe"]) {
    const m = menu(stored);
    assert.deepEqual(checkedOf(m), ["cmd"], stored);
    assert.equal(m.rows.length, 3, `no duplicate cmd row for ${stored}`);
  }
});

check("shellMenu: a unix path differing only in case is another program", () => {
  const m = T.shellMenu({ shell: "/usr/bin/Bash", defaultShell: "/bin/zsh", shells: [{ name: "bash", path: "/usr/bin/bash" }] });
  assert.deepEqual(m.rows.map((r) => [r.name, r.hint, r.checked]), [
    ["default", "zsh", false], ["bash", "", false], ["Bash", "other", true],
  ]);
});

check("shellMenu: a stored shell outside the list gets its own checked row, marked other", () => {
  const custom = "D:\\tools\\nu\\nu.exe";
  const m = menu(custom);
  assert.equal(m.label, "nu");
  assert.deepEqual(m.rows.at(-1), { name: "nu", hint: "other", path: custom, value: custom, checked: true });
  assert.deepEqual(checkedOf(m), ["nu"]);
});

check("shellMenu: settings not loaded yet still give one checked default row", () => {
  const m = T.shellMenu(null);
  assert.equal(m.label, "default", "names the row, like the browser's `automatic`");
  assert.deepEqual(m.rows, [{ name: "default", hint: "", path: "", value: "", checked: true }]);
});

check("clampHeight stays within [120, 70% of column]", () => {
  assert.equal(T.clampHeight(240, 1000), 240);
  assert.equal(T.clampHeight(50, 1000), 120, "raised to MIN_HEIGHT");
  assert.equal(T.clampHeight(900, 1000), 700);
  assert.equal(T.clampHeight(240.6, 1000), 241);
  assert.equal(T.clampHeight(500, 100), 70, "a column under 120/0.7 px: its 70 % share wins over MIN_HEIGHT");
  assert.equal(T.clampHeight(500, 0), 0, "no column: never below 0");
});

check("the height default lives only in TWEAK_DEFAULTS", () => {
  assert.equal("DEFAULT_HEIGHT" in T, false);
  const cw = {};
  // eslint-disable-next-line no-new-func
  new Function("window", readFileSync(join(root, "src/app/constants.js"), "utf8"))(cw);
  assert.equal(cw.TWEAK_DEFAULTS.terminalHeight, 240);
  assert.equal(T.clampHeight(cw.TWEAK_DEFAULTS.terminalHeight, 1000), 240, "the default is a valid height");
});

console.log(`terminal: ${passed} checks passed`);
