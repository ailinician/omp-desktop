#!/usr/bin/env node
// Regression script for examples/widgets/git.sh: a widget runs with the
// (untrusted) project as cwd, so a hostile `json.py` in the repo root must
// never be imported by the script's python helper.
// Run: node tests/test-widget-examples.mjs

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");

const available = (cmd) => spawnSync(cmd, ["--version"], { stdio: "ignore" }).status === 0;
const missing = ["bash", "python3", "git"].filter((c) => !available(c));
if (missing.length) {
  console.log(`test-widget-examples: skipped (missing: ${missing.join(", ")})`);
  process.exit(0);
}

const tmp = realpathSync(mkdtempSync(join(tmpdir(), "omp-widget-example-")));
try {
  const project = join(tmp, "project");
  const canary = join(tmp, "canary");
  const git = (...args) => {
    const r = spawnSync("git", ["-C", project, ...args], { encoding: "utf8" });
    assert.equal(r.status, 0, `git ${args.join(" ")}: ${r.stderr}`);
  };
  spawnSync("git", ["init", "-q", project], { stdio: "ignore" });
  git("config", "user.email", "t@example.com");
  git("config", "user.name", "t");
  git("config", "commit.gpgsign", "false");
  writeFileSync(join(project, "f.txt"), "x\n");
  git("add", "f.txt");
  git("commit", "-q", "-m", "init");
  // Hostile module: shadows the stdlib `json` if the cwd is on sys.path.
  writeFileSync(
    join(project, "json.py"),
    `open(${JSON.stringify(canary)}, "w").write("pwned")\nimport sys\nsys.exit(97)\n`,
  );

  const r = spawnSync("bash", [join(root, "examples/widgets/git.sh")], {
    cwd: project,
    env: { ...process.env, OMP_WIDGET_PROJECT: project },
    encoding: "utf8",
    timeout: 30000,
  });
  assert.equal(r.status, 0, `git.sh exit ${r.status}: ${r.stderr}`);
  assert.equal(existsSync(canary), false, "hostile json.py in the project was executed");
  const doc = JSON.parse(r.stdout);
  assert.equal(doc.version, 1);
  assert.ok(Array.isArray(doc.blocks) && doc.blocks.length > 0, "blocks present");
  console.log("test-widget-examples: 1 checks passed");
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
