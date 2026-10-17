#!/usr/bin/env node
// Regression script for src/app/accent.js — the `accent` tweak as the four
// CSS tokens the themes define, so an accent override also moves
// --accent-soft / --accent-glow / --accent-deep (focus rings, xterm
// selection) instead of leaving the theme preset's colour in them.
// Run: node tests/test-accent.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win  = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/accent.js"), "utf8"))(win);
const A = win.OMP_ACCENT;

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

check("accentTokens: all four tokens from a #rrggbb accent", () => {
  assert.deepEqual(A.accentTokens("#FF7AC6"), {
    "--accent": "#ff7ac6",
    "--accent-soft": "rgba(255, 122, 198, 0.12)",
    "--accent-glow": "rgba(255, 122, 198, 0.45)",
    "--accent-deep": "#bf5c95",
  });
});

check("accentTokens: #rgb shorthand expands", () => {
  assert.equal(A.accentTokens("#fff")["--accent"], "#ffffff");
  assert.equal(A.accentTokens(" #0F0 ")["--accent-soft"], "rgba(0, 255, 0, 0.12)");
});

check("accentTokens: the token names are the ones the themes define", () => {
  assert.deepEqual(Object.keys(A.accentTokens("#000000")), A.TOKEN_NAMES);
});

check("accentTokens: non-hex input gives null (theme tokens stay)", () => {
  for (const bad of ["", null, undefined, "red", "#12", "#12345", "rgb(1,2,3)", "#ggg"]) {
    assert.equal(A.accentTokens(bad), null, String(bad));
  }
});

check("sameColor: case and shorthand insensitive; non-hex never matches", () => {
  assert.equal(A.sameColor("#8AF0C8", "#8af0c8"), true);
  assert.equal(A.sameColor("#abc", "#AABBCC"), true);
  assert.equal(A.sameColor("#8AF0C8", "#C4FF3F"), false);
  assert.equal(A.sameColor("", "#8AF0C8"), false);
  assert.equal(A.sameColor("red", "red"), false);
});

console.log(`accent: ${passed} checks passed`);
