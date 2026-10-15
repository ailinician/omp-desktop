#!/usr/bin/env node
// Regression script for src/app/path-setting.js — the menu behind a
// Rust-owned program path setting (Tweaks: terminal shell, browser
// Chromium). The feature mappings over it are covered in test-terminal.mjs
// (`shellMenu`) and test-browser.mjs (`pathMenu`).

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// project-nav.js gives the menu its basename / normPath.
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/project-nav.js"), "utf8"))(win);
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/path-setting.js"), "utf8"))(win);
const P = win.OMP_PATH_SETTING;

let passed = 0;
function check(label, fn) {
  try { fn(); } catch (err) { console.error(`FAILED: ${label}`); throw err; }
  passed++;
}

const name = (p) => P.programName(p, "program");
const fallback = { name: "default", hint: "zsh", path: "/bin/zsh" };
const installed = [{ name: "bash", path: "/bin/bash" }, { name: "pwsh", path: "C:\\Program Files\\PowerShell\\7\\pwsh.exe", hint: "7" }];
const checkedOf = (m) => m.rows.filter((r) => r.checked).map((r) => r.name);

check("programName: basename without .exe, the fallback for none", () => {
  assert.equal(P.programName("/opt/google/chrome/chrome", "x"), "chrome");
  assert.equal(P.programName("C:\\Windows\\System32\\cmd.exe", "x"), "cmd");
  assert.equal(P.programName("", "x"), "x");
  assert.equal(P.programName(null, "x"), "x");
});

check("menu: nothing stored checks the fallback row, which names what runs", () => {
  const m = P.menu({ stored: "", name, fallback, installed });
  assert.equal(m.label, "zsh");
  assert.deepEqual(m.rows.map((r) => [r.name, r.hint, r.path, r.value, r.checked]), [
    ["default", "zsh", "/bin/zsh", "", true],
    ["bash", "", "/bin/bash", "/bin/bash", false],
    ["pwsh", "7", installed[1].path, installed[1].path, false],
  ]);
});

check("menu: a stored listed program is checked, Windows paths ignoring case and slashes", () => {
  const m = P.menu({ stored: "c:/program files/powershell/7/PWSH.EXE", name, fallback, installed });
  assert.deepEqual(checkedOf(m), ["pwsh"]);
  assert.equal(m.rows.length, 3, "no extra row");
  assert.equal(m.label, "PWSH");
});

check("menu: a stored path outside the list is appended once, marked other", () => {
  const m = P.menu({ stored: "/home/me/bin/fish", name, fallback, installed });
  assert.deepEqual(m.rows.at(-1), { name: "fish", hint: "other", path: "/home/me/bin/fish", value: "/home/me/bin/fish", checked: true });
  assert.deepEqual(checkedOf(m), ["fish"]);
  assert.equal(m.label, "fish");
});

check("menu: with nothing stored and nothing found the label is the fallback's name", () => {
  const m = P.menu({ stored: "", name, fallback: { name: "automatic", hint: "none found", path: "" }, installed: [] });
  assert.equal(m.label, "automatic");
  assert.deepEqual(m.rows, [{ name: "automatic", hint: "none found", path: "", value: "", checked: true }]);
});

console.log(`path-setting: ${passed} checks passed`);
