#!/usr/bin/env node
// Refreshes the vendored xterm.js files in src/ from the pinned devDependencies:
//
//   npm install && npm run vendor:xterm
//
// - src/xterm.js            ← @xterm/xterm/lib/xterm.js       (UMD → window.Terminal)
// - src/xterm.css           ← @xterm/xterm/css/xterm.css
// - src/xterm-addon-fit.js  ← @xterm/addon-fit/lib/addon-fit.js (UMD → window.FitAddon)
//
// Copied as is, apart from the trailing `//# sourceMappingURL=…` comment: the
// .map files are not vendored, so the webview would request a missing file
// (and the release CSP would have to cover it). Never hand-edit the outputs;
// bump the versions in package.json and re-run this script instead.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const files = [
  ["@xterm/xterm/lib/xterm.js", "xterm.js"],
  ["@xterm/xterm/css/xterm.css", "xterm.css"],
  ["@xterm/addon-fit/lib/addon-fit.js", "xterm-addon-fit.js"],
];

const sourceMapComment = /\n?\/\/# sourceMappingURL=\S+\s*$/;

mkdirSync(join(root, "src"), { recursive: true });
for (const [from, to] of files) {
  const text = readFileSync(join(root, "node_modules", from), "utf8").replace(sourceMapComment, "\n");
  writeFileSync(join(root, "src", to), text);
  console.log(`src/${to} ← ${from} (${text.length} bytes)`);
}
