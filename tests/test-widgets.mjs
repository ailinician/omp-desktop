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
check("non-primitive version is an error, not an exception", () => {
  const r = W.parseOutput('{"version":{"toString":null}}');
  assert.equal(r.ok, false);
  assert.match(r.error, /invalid version/);
});
check("string version is rejected", () => {
  const r = out({ version: "1", blocks: [] });
  assert.equal(r.ok, false);
  assert.match(r.error, /invalid version/);
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
const plugin = (over = {}) => ({ id: "p", title: "P", command: ["x"], matchPath: null, matchRemote: null, triggers: ["activate", "manual"], timeoutMs: 1000, intervalMs: undefined, enabled: true, ...over });
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
check("wantsTrigger interval is true only when intervalMs is set", () => {
  assert.equal(W.wantsTrigger(plugin(), "interval"), false);
  assert.equal(W.wantsTrigger(plugin({ intervalMs: 60000 }), "interval"), true);
  assert.equal(W.wantsTrigger(plugin({ intervalMs: 0 }), "interval"), false, "0 = no interval");
  assert.equal(W.wantsTrigger(plugin({ intervalMs: 60000, triggers: [] }), "interval"), true, "interval ignores triggers list");
});
check("tweakKey is namespaced", () => { assert.equal(W.tweakKey("git"), "widget:git"); });

// ── schedule ──────────────────────────────────────────────────────────────
new Function("window", readFileSync(join(root, "src/app/widgets-schedule.js"), "utf8"))(win);
const S = win.OMP_WIDGET_SCHEDULE;
const okResult = { ok: true, widget: { status: "ok", blocks: [], skipped: 0 }, durationMs: 5 };

check("start marks running and returns run=true", () => {
  const { state, seq, run } = S.start(S.EMPTY, "s1", "git", "activate");
  assert.equal(run, true);
  assert.equal(S.runOf(state, "s1", "git").state, "running");
  assert.equal(seq, 1);
});
check("a second start while running is coalesced into one queued trigger", () => {
  let { state } = S.start(S.EMPTY, "s1", "git", "activate");
  const r2 = S.start(state, "s1", "git", "turn_end"); state = r2.state;
  const r3 = S.start(state, "s1", "git", "manual"); state = r3.state;
  assert.equal(r2.run, false); assert.equal(r3.run, false);
  assert.equal(S.runOf(state, "s1", "git").queued, "manual", "latest trigger wins");
});
check("finish stores the result and hands back the queued trigger once", () => {
  let { state, seq } = S.start(S.EMPTY, "s1", "git", "activate");
  state = S.start(state, "s1", "git", "branch").state;
  const f = S.finish(state, "s1", "git", seq, okResult);
  assert.equal(f.next, "branch");
  assert.equal(S.runOf(f.state, "s1", "git").state, "ok");
  assert.equal(S.runOf(f.state, "s1", "git").queued, undefined);
});
check("a stale finish (old seq) is ignored", () => {
  let { state, seq: s1 } = S.start(S.EMPTY, "s1", "git", "activate");
  const f1 = S.finish(state, "s1", "git", s1, okResult); state = f1.state;
  const { state: st2, seq: s2 } = S.start(state, "s1", "git", "manual"); state = st2;
  const stale = S.finish(state, "s1", "git", s1, { ok: false, error: "old", durationMs: 1 });
  assert.equal(S.runOf(stale.state, "s1", "git").state, "running", "old seq must not overwrite");
  const fresh = S.finish(state, "s1", "git", s2, okResult);
  assert.equal(S.runOf(fresh.state, "s1", "git").state, "ok");
});
check("late result is keyed by the session it was started for", () => {
  let { state, seq } = S.start(S.EMPTY, "s1", "git", "activate");
  state = S.start(state, "s2", "git", "activate").state;
  const f = S.finish(state, "s1", "git", seq, { ok: false, error: "boom", durationMs: 1 });
  assert.equal(S.runOf(f.state, "s1", "git").state, "error");
  assert.equal(S.runOf(f.state, "s2", "git").state, "running");
});
check("an error keeps lastOkAt from the previous success and drops the widget", () => {
  let { state, seq } = S.start(S.EMPTY, "s1", "git", "activate");
  state = S.finish(state, "s1", "git", seq, { ...okResult, at: 1000 }).state;
  const again = S.start(state, "s1", "git", "manual"); state = again.state;
  state = S.finish(state, "s1", "git", again.seq, { ok: false, error: "x", durationMs: 1, at: 2000 }).state;
  const r = S.runOf(state, "s1", "git");
  assert.equal(r.state, "error"); assert.equal(r.lastOkAt, 1000); assert.equal(r.widget, undefined);
});
check("a running run keeps the previous widget for the stale display", () => {
  let { state, seq } = S.start(S.EMPTY, "s1", "git", "activate");
  state = S.finish(state, "s1", "git", seq, okResult).state;
  state = S.start(state, "s1", "git", "manual").state;
  assert.ok(S.runOf(state, "s1", "git").widget, "widget kept while running");
});
check("prune drops closed sessions", () => {
  let { state } = S.start(S.EMPTY, "s1", "git", "activate");
  state = S.start(state, "s2", "git", "activate").state;
  state = S.prune(state, ["s2"]);
  assert.equal(S.runOf(state, "s1", "git"), undefined);
  assert.ok(S.runOf(state, "s2", "git"));
});
check("turnEnded fires only for streaming true to false within one tab", () => {
  assert.equal(S.turnEnded({ sessionId: "a", streaming: true }, { sessionId: "a", streaming: false }), true);
  assert.equal(S.turnEnded({ sessionId: "a", streaming: true }, { sessionId: "b", streaming: false }), false, "tab switch");
  assert.equal(S.turnEnded({ sessionId: "a", streaming: false }, { sessionId: "a", streaming: true }), false);
  assert.equal(S.turnEnded({ sessionId: "a", streaming: true }, { sessionId: "a", streaming: true }), false);
});
check("dueNow is true when run is absent or last run is older than interval", () => {
  assert.equal(S.dueNow(undefined, 60000, 100000), true);
  assert.equal(S.dueNow({ at: 39000 }, 60000, 100000), true, "100000 - 39000 = 61000 >= 60000");
  assert.equal(S.dueNow({ at: 40000 }, 60000, 100000), true, "100000 - 40000 = 60000 >= 60000");
  assert.equal(S.dueNow({ at: 41000 }, 60000, 100000), false);
});
check("dueNow is false while a run is in flight", () => {
  assert.equal(S.dueNow({ state: "running", at: 1000 }, 60000, 100000), false, "in-flight counts as fresh");
  assert.equal(S.dueNow({ state: "ok", at: 1000 }, 60000, 100000), true, "ok with old at is due");
});
check("dueNow is false for a first run still in flight (no result time yet)", () => {
  const { state } = S.start(S.EMPTY, "s1", "git", "activate");
  assert.equal(S.dueNow(S.runOf(state, "s1", "git"), 60000, 100000), false);
});

// ── scalar coercion ───────────────────────────────────────────────────────
check("kv.v coerces string, number, boolean; drops null/object", () => {
  const r = out({ version: 1, blocks: [{ type: "kv", rows: [
    { k: "ahead", v: 3 },
    { k: "x", v: true },
    { k: "y", v: false },
    { k: "bad", v: {} },
    { k: "nul", v: null },
    { k: "ok", v: "str" },
  ] }] });
  assert.deepEqual(r.widget.blocks[0].rows, [
    { k: "ahead", v: "3" },
    { k: "x", v: "true" },
    { k: "y", v: "false" },
    { k: "ok", v: "str" },
  ]);
});
check("status.value coerces scalars", () => {
  const r = out({ version: 1, blocks: [{ type: "status", label: "x", value: 42 }] });
  assert.equal(r.widget.blocks[0].value, "42");
});
check("item label and detail coerce scalars; null/object labels drop the item", () => {
  const r = out({ version: 1, blocks: [{ type: "list", items: [
    { label: 3, detail: true },
    { label: {}, detail: "dropped" },
    { label: null },
    { label: "ok", detail: {} },
  ] }] });
  assert.deepEqual(r.widget.blocks[0].items, [
    { label: "3", tone: "neutral", detail: "true" },
    { label: "ok", tone: "neutral" },
  ]);
});

// ── tone info ─────────────────────────────────────────────────────────────
check("info tone is preserved", () => {
  const r = out({ version: 1, status: "info", blocks: [{ type: "status", label: "x", value: "y", tone: "info" }] });
  assert.equal(r.widget.status, "info");
  assert.equal(r.widget.blocks[0].tone, "info");
});
check("unknown tone still falls back to neutral", () => {
  assert.equal(out({ version: 1, status: "purple", blocks: [] }).widget.status, "neutral");
});

// ── title on list and kv ──────────────────────────────────────────────────
check("title on list and kv is preserved and clipped", () => {
  const long = "t".repeat(300);
  const r = out({ version: 1, blocks: [
    { type: "list", items: [{ label: "a" }], title: "Section A" },
    { type: "kv", rows: [{ k: "x", v: "y" }], title: long },
  ] });
  assert.equal(r.widget.blocks[0].title, "Section A");
  assert.equal(r.widget.blocks[1].title.length, 200);
});
check("absent title means no field", () => {
  const r = out({ version: 1, blocks: [{ type: "list", items: [{ label: "a" }] }] });
  assert.equal("title" in r.widget.blocks[0], false);
});

// ── item time ─────────────────────────────────────────────────────────────
check("item time: ISO string and epoch seconds become epoch ms; invalid is dropped", () => {
  const r = out({ version: 1, blocks: [{ type: "list", items: [
    { label: "a", time: "2026-01-15T12:00:00Z" },
    { label: "b", time: 1736942400 },
    { label: "c", time: "not a date" },
    { label: "d", time: null },
    { label: "e", time: 1e20 },
    { label: "f", time: {} },
  ] }] });
  const times = r.widget.blocks[0].items.map(i => i.time);
  assert.deepEqual(times, [Date.parse("2026-01-15T12:00:00Z"), 1736942400000, undefined, undefined, undefined, undefined]);
  assert.equal(r.widget.blocks[0].items.length, 6, "a bad time never drops the item");
});

// ── relativeTime ──────────────────────────────────────────────────────────
const NOW = Date.parse("2026-06-15T12:00:00Z");
const MIN = 60_000, HOUR = 3_600_000, DAY = 86_400_000;
check("relativeTime: past minutes, hours, days (floored)", () => {
  assert.equal(W.relativeTime(NOW - 5 * MIN, NOW), "5m");
  assert.equal(W.relativeTime(NOW - 3 * HOUR, NOW), "3h");
  assert.equal(W.relativeTime(NOW - 2 * DAY, NOW), "2d");
  assert.equal(W.relativeTime(NOW - (HOUR - 1), NOW), "59m", "floors, never rounds up to 60m");
  assert.equal(W.relativeTime(NOW - (DAY - 1), NOW), "23h");
});
check("relativeTime: future gets an in prefix", () => {
  assert.equal(W.relativeTime(NOW + 12 * MIN, NOW), "in 12m");
  assert.equal(W.relativeTime(NOW + 3 * HOUR, NOW), "in 3h");
  assert.equal(W.relativeTime(NOW + 2 * DAY, NOW), "in 2d");
});
check("relativeTime: under a minute is now, unit boundaries are exact", () => {
  assert.equal(W.relativeTime(NOW - (MIN - 1), NOW), "now");
  assert.equal(W.relativeTime(NOW + 30_000, NOW), "now");
  assert.equal(W.relativeTime(NOW - MIN, NOW), "1m");
  assert.equal(W.relativeTime(NOW - HOUR, NOW), "1h");
  assert.equal(W.relativeTime(NOW - DAY, NOW), "1d");
});
check("relativeTime: 30 days is still days, beyond is the UTC date (past and future)", () => {
  assert.equal(W.relativeTime(NOW - 30 * DAY, NOW), "30d");
  assert.equal(W.relativeTime(NOW - 30 * DAY - 1, NOW), "2026-05-16");
  assert.equal(W.relativeTime(NOW + 31 * DAY, NOW), "2026-07-16");
});

// ── insert on text block ──────────────────────────────────────────────────
check("text block insert is preserved and clipped", () => {
  const r = out({ version: 1, blocks: [
    { type: "text", text: "hi", insert: "insert me" },
    { type: "text", text: "no insert" },
    { type: "text", text: "long", insert: "x".repeat(3000) },
  ] });
  assert.equal(r.widget.blocks[0].insert, "insert me");
  assert.equal("insert" in r.widget.blocks[1], false);
  assert.equal(r.widget.blocks[2].insert.length, 2000);
});

// ── scope / schedule key mapping ──────────────────────────────────────────
check("keyFor maps global plugins to the pseudo-session and tab plugins to their session", () => {
  assert.equal(S.keyFor({ scope: "global" }, "s1"), "__global__");
  assert.equal(S.keyFor({ scope: "tab" }, "s1"), "s1");
  assert.equal(S.keyFor({}, "s1"), "s1", "absent scope = tab");
});
check("a global run is shared across tabs under the pseudo-session key", () => {
  const key = (sid) => S.keyFor({ scope: "global" }, sid);
  let { state, seq } = S.start(S.EMPTY, key("s1"), "g", "activate");
  state = S.finish(state, key("s1"), "g", seq, { ...okResult, at: 5 }).state;
  assert.equal(S.runOf(state, key("s2"), "g").at, 5, "tab s2 sees the result started from s1");
  assert.equal(S.start(state, key("s2"), "g", "manual").run, true);
});
check("prune never drops __global__ but still drops closed tabs", () => {
  let { state } = S.start(S.EMPTY, "__global__", "git", "activate");
  state = S.start(state, "s1", "git", "activate").state;
  state = S.prune(state, []);
  assert.ok(S.runOf(state, "__global__", "git"));
  assert.equal(S.runOf(state, "s1", "git"), undefined);
});
check("activateDue: tab plugins always run; global ones only without a fresh result", () => {
  const g = { scope: "global", intervalMs: 60000 };
  assert.equal(S.activateDue({ scope: "tab", intervalMs: 60000 }, { state: "ok", at: 99000 }, 100000), true);
  assert.equal(S.activateDue(g, undefined, 100000), true, "no result yet");
  assert.equal(S.activateDue(g, { state: "ok", at: 99000 }, 100000), false, "fresh result skips");
  assert.equal(S.activateDue(g, { state: "ok", at: 40000 }, 100000), true, "stale result runs");
  assert.equal(S.activateDue(g, { state: "running" }, 100000), false, "first run in flight skips");
  assert.equal(S.activateDue({ scope: "global" }, { state: "ok", at: 99999 }, 100000), true, "no intervalMs = always");
});

console.log(`test-widgets: ${passed} checks passed`);
