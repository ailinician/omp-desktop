#!/usr/bin/env node
// Regression script for src/app/widgets.js — the rail widget plugin
// contract: output schema v1 validation and capping, plugin/project
// matching, trigger filtering, and the composer insert text.
// Run: node tests/test-widgets.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/widgets.js"), "utf8"))(win);
const W = win.OMP_WIDGETS;

let passed = 0;
function check(label, fn) {
  try { fn(); } catch (err) { console.error(`FAILED: ${label}`); throw err; }
  passed++;
}
const out = (o) => W.parseOutput(JSON.stringify(o));

// ── parseOutput ───────────────────────────────────────────────────────────
check("valid v1 output renders its blocks and default status", () => {
  const r = out({ version: 1, blocks: [{ type: "text", text: "hi" }] });
  assert.equal(r.ok, true);
  assert.equal(r.widget.status, "neutral");
  assert.deepEqual(r.widget.blocks, [{ type: "text", text: "hi" }]);
  assert.equal(r.widget.skipped, 0);
});
check("non-JSON stdout is an error naming the first bytes", () => {
  const r = W.parseOutput("not json at all");
  assert.equal(r.ok, false);
  assert.match(r.error, /not JSON.*not json at all/);
});
check("version 2 is rejected with the supported version named", () => {
  const r = out({ version: 2, blocks: [] });
  assert.equal(r.ok, false);
  assert.match(r.error, /version 2.*supports 1/);
});
check("missing blocks array is an error", () => {
  assert.equal(out({ version: 1 }).ok, false);
});
check("unknown block types are skipped and counted", () => {
  const r = out({ version: 1, blocks: [{ type: "hologram" }, { type: "text", text: "x" }] });
  assert.equal(r.widget.blocks.length, 1);
  assert.equal(r.widget.skipped, 1);
});
check("unknown status/tone fall back to neutral", () => {
  const r = out({ version: 1, status: "purple", blocks: [{ type: "status", label: "a", value: "b", tone: "loud" }] });
  assert.equal(r.widget.status, "neutral");
  assert.equal(r.widget.blocks[0].tone, "neutral");
});
check("strings are capped and lists are truncated", () => {
  const items = Array.from({ length: 80 }, (_, i) => ({ label: "x".repeat(500) + i }));
  const r = out({ version: 1, blocks: [{ type: "list", items }, { type: "text", text: "y".repeat(5000) }] });
  assert.equal(r.widget.blocks[0].items.length, 50);
  assert.equal(r.widget.blocks[0].items[0].label.length, 200);
  assert.equal(r.widget.blocks[1].text.length, 2000);
});
check("more than 32 blocks are truncated", () => {
  const blocks = Array.from({ length: 40 }, () => ({ type: "text", text: "t" }));
  assert.equal(out({ version: 1, blocks }).widget.blocks.length, 32);
});
check("number and progress coerce to finite numbers or are skipped", () => {
  const r = out({ version: 1, blocks: [
    { type: "number", label: "n", value: "12" },
    { type: "number", label: "bad", value: "NaN" },
    { type: "progress", label: "p", value: 2, max: 0 },
  ] });
  assert.equal(r.widget.blocks[0].value, 12);
  assert.equal(r.widget.blocks.length, 2, "NaN number skipped");
  assert.equal(r.widget.blocks[1].max, 1, "max<1 becomes 1");
  assert.equal(r.widget.skipped, 1);
});
check("kv rows need string k and v", () => {
  const r = out({ version: 1, blocks: [{ type: "kv", rows: [{ k: "a", v: "b" }, { k: 1 }, "junk"] }] });
  assert.deepEqual(r.widget.blocks[0].rows, [{ k: "a", v: "b" }]);
});

check("inherited Object.prototype keys are not block types", () => {
  const r = out({ version: 1, blocks: [{ type: "__proto__" }, { type: "constructor" }, { type: "toString" }, { type: "text", text: "ok" }] });
  assert.deepEqual(r.widget.blocks, [{ type: "text", text: "ok" }]);
  assert.equal(r.widget.skipped, 3);
});
check("non-number non-string numeric fields never throw", () => {
  const r = out({ version: 1, blocks: [
    { type: "number", label: "n", value: { toString: null } },
    { type: "progress", label: "p", value: { toString: null } },
    { type: "progress", label: "q", value: 1, max: { toString: null } },
    { type: "text", text: "ok" },
  ] });
  assert.deepEqual(r.widget.blocks, [{ type: "progress", label: "q", value: 1, max: 1 }, { type: "text", text: "ok" }]);
  assert.equal(r.widget.skipped, 2);
});
check("link without label caps the label but keeps the full url", () => {
  const url = "https://example.com/" + "a".repeat(480);
  const r = out({ version: 1, blocks: [{ type: "link", url }] });
  assert.equal(r.widget.blocks[0].label.length, 200);
  assert.equal(r.widget.blocks[0].url.length, 500);
});
// ── safeUrl / insertText ──────────────────────────────────────────────────
check("only http(s) urls are opened", () => {
  assert.equal(W.safeUrl("https://x.y/z"), "https://x.y/z");
  assert.equal(W.safeUrl("javascript:alert(1)"), null);
  assert.equal(W.safeUrl("file:///etc/passwd"), null);
  assert.equal(W.safeUrl(undefined), null);
});
check("insert text is capped and NUL-free", () => {
  const t = W.insertText({ insert: "a\u0000b" + "c".repeat(5000) });
  assert.equal(t.length, 2000);
  assert.equal(t.includes("\u0000"), false);
  assert.equal(t.startsWith("abc"), true);
});
check("insert falls back to label and url", () => {
  assert.equal(W.insertText({ label: "MR !1", url: "https://g/1" }), "MR !1 https://g/1");
  assert.equal(W.insertText({ label: "x" }), "x");
  assert.equal(W.insertText({}), "");
});

// ── applicable / wantsTrigger ─────────────────────────────────────────────
const plugin = (over = {}) => ({ id: "p", title: "P", command: ["x"], matchPath: null, matchRemote: null, triggers: ["activate", "manual"], timeoutMs: 1000, enabled: true, ...over });
check("disabled plugins never apply", () => {
  assert.deepEqual(W.applicable([plugin({ enabled: false })], { project: "/a", remote: "" }), []);
});
check("path prefix and remote substring must both match when given", () => {
  const ps = [plugin({ id: "a", matchPath: "/Users/me/GIT" }), plugin({ id: "b", matchRemote: "gitlab.example" }), plugin({ id: "c", matchPath: "/Users/me/GIT", matchRemote: "gitlab.example" })];
  const ids = (ctx) => W.applicable(ps, ctx).map(p => p.id);
  assert.deepEqual(ids({ project: "/Users/me/GIT/x", remote: "git@gitlab.example:g/x.git" }), ["a", "b", "c"]);
  assert.deepEqual(ids({ project: "/tmp/x", remote: "git@gitlab.example:g/x.git" }), ["b"]);
  assert.deepEqual(ids({ project: "/Users/me/GIT/x", remote: "" }), ["a"]);
});
check("path match is prefix on path separators, not on characters", () => {
  assert.deepEqual(W.applicable([plugin({ matchPath: "/Users/me/GIT" })], { project: "/Users/me/GITHUB/x", remote: "" }), []);
});
check("matchPath \"/\" matches any absolute project, not relative ones", () => {
  const ids = (project) => W.applicable([plugin({ matchPath: "/" })], { project, remote: "" }).length;
  assert.equal(ids("/a/b"), 1);
  assert.equal(ids("/"), 1);
  assert.equal(ids("relative/x"), 0);
});
check("wantsTrigger honours the plugin's list and always allows manual", () => {
  assert.equal(W.wantsTrigger(plugin({ triggers: ["branch"] }), "branch"), true);
  assert.equal(W.wantsTrigger(plugin({ triggers: ["branch"] }), "turn_end"), false);
  assert.equal(W.wantsTrigger(plugin({ triggers: [] }), "manual"), true);
});
check("tweakKey is namespaced", () => { assert.equal(W.tweakKey("git"), "widget:git"); });

console.log(`test-widgets: ${passed} checks passed`);
