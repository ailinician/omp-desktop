#!/usr/bin/env node
// Regression script for buildToolStartCard / finalizeToolCard (src/adapter.js):
// the bash branch's display lines and the text its copy button puts on the
// clipboard (issue #33) — the card shows omp's result text as streamed,
// notices included; the copy text drops the model-facing notices omp appends
// (exit code, wall time, artifact footer) — omp's failure flag on any card
// (#43), and the edit card's file paths (the git browser's open-file
// buttons) across omp's five edit modes.
// Run: node tests/test-tool-output.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const ctx = vm.createContext({ console });
ctx.window = ctx;
vm.runInContext(readFileSync(join(root, "src", "adapter.js"), "utf8"), ctx, { filename: "adapter.js" });
const { buildToolStartCard, finalizeToolCard } = ctx;

let passed = 0;
function check(label, fn) {
  try { fn(); passed++; }
  catch (err) { console.error(`FAIL ${label}\n${err.message}`); process.exitCode = 1; }
}

const card = { kind: "tool", tool: "bash", status: "running", output: [{ line: "streamed", color: "fg-3" }] };
const finish = (text, details = {}, extraContent = []) =>
  finalizeToolCard(card, { result: { content: [{ type: "text", text }, ...extraContent], details } });
const shown = (c) => c.output.map((l) => l.line).join("\n");

// Result texts below are shaped like real omp 18 frames, e.g.
// "fast-one\n\n\nWall time: 0.04 seconds" with wallTimeMs 39.22144800000024.
check("the wall-time notice stays on the card but not in the copy text", () => {
  const c = finish("alpha\n\tbeta <x>\n\n\nWall time: 0.04 seconds", { wallTimeMs: 39.22144800000024 });
  assert.equal(c.outputText, "alpha\n\tbeta <x>");
  assert.equal(shown(c), "alpha\n\tbeta <x>\n\n\nWall time: 0.04 seconds");
});

check("a failed command keeps its exit code on the card, not in the copy text", () => {
  const c = finish("boom\n\n\nWall time: 1.50 seconds\n\nCommand exited with code 2", { wallTimeMs: 1500, exitCode: 2 });
  assert.equal(c.outputText, "boom");
  assert.match(shown(c), /Command exited with code 2$/);
});

check("the raw-output artifact footer is dropped from the copy text", () => {
  const c = finish("big\n\nWall time: 3.00 seconds\n[raw output: artifact://17]", { wallTimeMs: 3000 });
  assert.equal(c.outputText, "big");
});

check("an auto-backgrounded command's job note is dropped from the copy text", () => {
  const c = finish("building…\nstep 1\n\nBackgrounded as job bg-7; its output is injected into the conversation as a follow-up the moment it finishes.",
    { async: { state: "running", jobId: "bg-7", type: "bash" } });
  assert.equal(c.outputText, "building…\nstep 1");
});

check("output that merely looks like a notice is kept", () => {
  const c = finish("Wall time: 9.99 seconds\nCommand exited with code 5\n\nWall time: 0.01 seconds", { wallTimeMs: 10 });
  assert.equal(c.outputText, "Wall time: 9.99 seconds\nCommand exited with code 5");
});

check("a silent command offers nothing to copy", () => {
  const c = finish("(no output)\n\nWall time: 0.00 seconds", { wallTimeMs: 0.4 });
  assert.equal(c.outputText, undefined);
});

check("an empty result keeps the streamed lines", () => {
  const c = finish("");
  assert.equal(c.outputText, undefined);
  assert.equal(shown(c), "streamed");
});

check("the card shows the last 20 lines; the copy text keeps them all", () => {
  const lines = Array.from({ length: 35 }, (_, i) => `line ${i + 1}`);
  const c = finish(lines.join("\n") + "\n\n\nWall time: 0.04 seconds", { wallTimeMs: 40 });
  assert.equal(c.outputText, lines.join("\n"));
  assert.equal(shown(c), [...lines.slice(-17), "", "", "Wall time: 0.04 seconds"].join("\n"));
});

check("non-text result blocks are ignored", () => {
  const c = finish("png written", {}, [{ type: "image", data: "AAAA", mimeType: "image/png" }]);
  assert.equal(c.outputText, "png written");
});

check("a call omp reports as failed is marked, so a folded card stays open (#43)", () => {
  const event = (isError) => ({ result: { content: [{ type: "text", text: "boom" }], details: { exitCode: 2 } }, isError });
  assert.equal(finalizeToolCard(card, event(true)).isError, true);
  assert.equal(finalizeToolCard(card, event(false)).isError, false);
});

// Edit cards: the paths the git browser opens. Shapes are omp 18's: the
// start frame's args per edit mode, the end frame's EditToolDetails
// (`path` for one file, `perFileResults` for several), as aggregateDetails
// in oh-my-pi's packages/coding-agent/src/edit/index.ts builds them.
const start = (args) => buildToolStartCard({ toolName: "edit", toolCallId: "t1", args, intent: "x" }, "12:00");
const end = (c, details) => finalizeToolCard(c, { result: { content: [{ type: "text", text: "ok" }], details } });
// Arrays made inside the vm context have their own Array prototype; strict
// deepEqual compares prototypes, so copy them into this realm first.
const arr = (a) => Array.from(a);
check("hashline mode: the [path#TAG] headers name the files while the edit runs", () => {
  const c = start({ input: "[src/math.js#0000]\nPUT >$:\n+x\n[src/math.js#0000]\nPUT 1.=1:\n+y\n[README.md#A1B2]\nPUT >$:\n+z" });
  assert.deepEqual(arr(c.targets), ["src/math.js", "README.md"]);
  assert.equal(c.target, "src/math.js, README.md");
});

check("apply_patch and sloppy modes: their file headers name the files", () => {
  const patch = start({ input: "*** Begin Patch\n*** Add File: hello.txt\n+Hello\n*** Update File: src/app.py\n*** Move to: src/main.py\n@@ def greet():\n-a\n+b\n*** Delete File: obsolete.txt\n*** End Patch\n" });
  assert.deepEqual(arr(patch.targets), ["hello.txt", "src/app.py", "obsolete.txt"]);
  // sloppy: ` all` is a flag, a bare `all` or nothing continues the previous
  // file, a JSON-quoted path is unquoted, CRLF and a header-only line are fine.
  const sloppy = start({ input: "*** Edit File: src/a.ts all\n<<<<<<< SEARCH\nfoo\n=======\nbar\n>>>>>>> REPLACE\n*** Edit File: all\n  *** edit file: lib/b.ts\r\n*** Edit File: \n*** Edit File: \"with space.ts\"\n*** Edit File: tall\n*** Add File: \n" });
  assert.deepEqual(arr(sloppy.targets), ["src/a.ts", "lib/b.ts", "with space.ts", "tall"]);
});

check("replace and patch modes: the path argument; no file known otherwise", () => {
  assert.deepEqual(arr(start({ path: "src/c.ts", old_string: "a", new_string: "b" }).targets), ["src/c.ts"]);
  assert.deepEqual(arr(start({ input: "nothing that names a file" }).targets), []);
  assert.equal(start({ input: "nothing that names a file" }).target, "");
});

check("a finished single-file edit takes the absolute path from its details, whatever the mode", () => {
  const c = end(start({ input: "[src/math.js#0000]\nPUT >$:\n+x" }), { diff: " 1|a\n+2|x", path: "/tmp/p/src/math.js", op: "update" });
  assert.deepEqual(arr(c.targets), ["/tmp/p/src/math.js"]);
  assert.equal(c.target, "/tmp/p/src/math.js");
  assert.equal(c.adds, 1);
});

check("a finished multi-file edit lists every perFileResults path; one without details keeps the start paths", () => {
  const multi = end(start({ input: "x" }), { diff: "", perFileResults: [{ path: "/tmp/p/a.ts", diff: "+1|a" }, { path: "/tmp/p/b.ts", diff: "" }] });
  assert.deepEqual(arr(multi.targets), ["/tmp/p/a.ts", "/tmp/p/b.ts"]);
  assert.equal(multi.target, "/tmp/p/a.ts, /tmp/p/b.ts");
  const kept = end(start({ path: "src/c.ts" }), { diff: "" });
  assert.deepEqual(arr(kept.targets), ["src/c.ts"]);
});

// Read and write cards (issue #39): a read result's image blocks (omp sends
// `[{type:"text"}, {type:"image", data, mimeType}]` when the model takes
// images) become thumbnails; a write card names its file for the git browser.
check("a read card keeps its result's images, at most four, and nothing else", () => {
  const read = buildToolStartCard({ toolName: "read", toolCallId: "r1", args: { path: "shot.png" } }, "12:00");
  const img = (n) => ({ type: "image", data: `AAA${n}`, mimeType: "image/png" });
  const done = finalizeToolCard(read, { result: { content: [
    { type: "text", text: "Read image file [image/png]" }, img(1), { type: "image", data: 7 }, img(2), img(3), img(4), img(5),
  ], details: {} } });
  assert.deepEqual(arr(done.images).map((i) => i.data), ["AAA1", "AAA2", "AAA3", "AAA4"]);
  assert.equal(done.images[0].mimeType, "image/png");
  const text = finalizeToolCard(read, { result: { content: [{ type: "text", text: "1|a" }], details: { lines: 1 } } });
  assert.equal(text.images, undefined);
  assert.equal(text.summary, "1 lines");
});

check("a write card names its file and takes the size from omp's result text", () => {
  const write = buildToolStartCard({ toolName: "write", toolCallId: "w1", args: { path: "tests/x.mjs", content: "x" } }, "12:00");
  assert.deepEqual(arr(write.targets), ["tests/x.mjs"]);
  const done = finalizeToolCard(write, { result: { content: [{ type: "text", text: "Successfully wrote 1946 bytes to tests/x.mjs" }], details: {} } });
  assert.equal(done.bytes, 1946);
  assert.deepEqual(arr(done.targets), ["tests/x.mjs"]);
  const failed = finalizeToolCard(write, { result: { content: [{ type: "text", text: "EACCES" }] }, isError: true });
  assert.equal(failed.bytes, undefined);
});

console.log(`tool-output: ${passed} checks passed`);
