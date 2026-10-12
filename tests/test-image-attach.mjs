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

check("a stray drop is refused for a file anywhere, for a link only off an editable field", () => {
  const fileDrag = { types: ["Files"] };
  const linkDrag = { types: ["text/uri-list", "text/plain"] };
  const textDrag = { types: ["text/plain", "text/html"] };
  assert.equal(I.refusesStrayDrop(fileDrag, false), true);
  assert.equal(I.refusesStrayDrop(fileDrag, true), true, "a file never drops into a field as navigation");
  assert.equal(I.refusesStrayDrop(linkDrag, false), true, "a link dropped on the chat would open the system browser");
  assert.equal(I.refusesStrayDrop({ types: domStringList("text/uri-list") }, false), true);
  assert.equal(I.refusesStrayDrop(linkDrag, true), false, "a link dropped into the textarea goes in as text");
  assert.equal(I.refusesStrayDrop(textDrag, false), false, "a text selection keeps the browser's handling");
  assert.equal(I.refusesStrayDrop(null, false), false);
});

// Replays dragenter/dragleave/drop events that reached the drop zone and
// returns the hover after each one.
const hoverTrace = events => {
  let cur = null;
  return events.map(([type, target]) => (cur = I.dropHoverAfter(cur, type, target)));
};

check("crossing children inside the drop zone keeps the hover; leaving it ends it", () => {
  const zone = {}, textarea = {}, button = {};
  // Engines fire dragenter on the new element before dragleave on the old.
  const trace = hoverTrace([
    ["dragenter", zone],
    ["dragenter", textarea], ["dragleave", zone],
    ["dragenter", button], ["dragleave", textarea],
    ["dragenter", zone], ["dragleave", button],
    ["dragleave", zone],
  ]);
  assert.deepEqual(trace.map(h => h !== null), [true, true, true, true, true, true, true, false]);
});

check("a leave lost to a child removed mid-drag does not leave the hover stuck", () => {
  const zone = {}, sendButton = {}, abortButton = {};
  // The hovered send button unmounts when a turn starts: its dragleave goes
  // to the detached node and never reaches the zone.
  const trace = hoverTrace([
    ["dragenter", zone],
    ["dragenter", sendButton], ["dragleave", zone],
    ["dragenter", abortButton],
    ["dragenter", zone], ["dragleave", abortButton],
    ["dragleave", zone],
  ]);
  assert.equal(trace.at(-1), null);
});

check("a drop ends the hover; Escape over a child does too", () => {
  const zone = {}, textarea = {};
  assert.equal(hoverTrace([["dragenter", zone], ["dragenter", textarea], ["dragleave", zone], ["drop", textarea]]).at(-1), null);
  assert.equal(hoverTrace([["dragenter", zone], ["dragenter", textarea], ["dragleave", zone], ["dragleave", textarea]]).at(-1), null);
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
