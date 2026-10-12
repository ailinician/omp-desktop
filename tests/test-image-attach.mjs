#!/usr/bin/env node
// Regression script for src/app/image-attach.js — which drags the composer
// takes as an image drop (#50), and which Files a paste/drop DataTransfer
// carries.
// Run: node tests/test-image-attach.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const src  = readFileSync(join(root, "src/app/image-attach.js"), "utf8");
const win  = {};
// eslint-disable-next-line no-new-func
new Function("window", src)(win);
const I = win.OMP_IMAGES;

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

const file = (name, type) => ({ name, type });
const fileItem = f => ({ kind: "file", type: f.type, getAsFile: () => f });
// Older WebKit hands `types` over as a DOMStringList, not an array.
const domStringList = (...xs) => ({ length: xs.length, ...xs, item: i => xs[i], contains: x => xs.includes(x) });

check("a drag from the file manager is a file drag; a text or link drag is not", () => {
  assert.equal(I.carriesFiles({ types: ["Files"] }), true);
  assert.equal(I.carriesFiles({ types: domStringList("Files", "text/uri-list") }), true);
  assert.equal(I.carriesFiles({ types: ["text/plain", "text/html"] }), false, "a text selection keeps the textarea's own drop");
  assert.equal(I.carriesFiles({ types: ["text/uri-list"] }), false);
  assert.equal(I.carriesFiles(null), false);
  assert.equal(I.carriesFiles({}), false);
});

check("a drop yields only its images, from items", () => {
  const png = file("a.png", "image/png");
  const pdf = file("b.pdf", "application/pdf");
  const dt = { items: [fileItem(png), fileItem(pdf), { kind: "string", type: "text/plain" }], files: [png, pdf] };
  assert.deepEqual(I.imageFilesFromTransfer(dt), [png], "no duplicate from `files`, no pdf");
});

check("an engine that fills only `files` still yields the images", () => {
  const jpg = file("c.jpg", "image/jpeg");
  assert.deepEqual(I.imageFilesFromTransfer({ items: [], files: [jpg, file("d.txt", "text/plain")] }), [jpg]);
  assert.deepEqual(I.imageFilesFromTransfer({ files: [file("d.txt", "text/plain")] }), []);
  assert.deepEqual(I.imageFilesFromTransfer(null), []);
});

console.log(`image-attach: ${passed} checks passed`);
