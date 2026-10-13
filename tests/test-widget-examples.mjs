#!/usr/bin/env node
// Regression script for examples/widgets/git.sh: a widget runs with the
// (untrusted) project as cwd, so a hostile `json.py` in the repo root must
// never be imported by the script's python helper, and the output must pass
// the desktop's own parser (src/app/widgets.js parseOutput) with no block
// skipped — on a branch with an upstream, without one, and on a detached HEAD.
// Run: node tests/test-widget-examples.mjs

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/widgets.js"), "utf8"))(win);
const { parseOutput } = win.OMP_WIDGETS;

// Hermetic env: no inherited GIT_* (GIT_DIR, GIT_INDEX_FILE, …), no user or
// system git config (hooksPath, signing, templates).
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("GIT_")));
Object.assign(env, {
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@example.com",
  GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@example.com",
});

const available = (cmd) => spawnSync(cmd, ["--version"], { stdio: "ignore", env }).status === 0;
const missing = ["bash", "python3", "git"].filter((c) => !available(c));
if (missing.length) {
  console.log(`test-widget-examples: skipped (missing: ${missing.join(", ")})`);
  process.exit(0);
}

let passed = 0;
function check(label, fn) {
  fn();
  passed++;
  console.log(`  ok  ${label}`);
}

const tmp = realpathSync(mkdtempSync(join(tmpdir(), "omp-widget-example-")));
try {
  const project = join(tmp, "project");
  const upstream = join(tmp, "upstream.git");
  const canary = join(tmp, "canary");
  const git = (...args) => {
    const r = spawnSync("git", ["-C", project, ...args], { encoding: "utf8", env });
    assert.equal(r.status, 0, `git ${args.join(" ")}: ${r.stderr}`);
    return r.stdout.trim();
  };
  const init = spawnSync("git", ["init", "-q", "-b", "main", project], { encoding: "utf8", env });
  assert.equal(init.status, 0, `git init: ${init.stderr}`);
  writeFileSync(join(project, "f.txt"), "x\n");
  git("add", "f.txt");
  git("commit", "-q", "-m", "init");
  // Hostile module: shadows the stdlib `json` if the cwd is on sys.path.
  writeFileSync(
    join(project, "json.py"),
    `open(${JSON.stringify(canary)}, "w").write("pwned")\nimport sys\nsys.exit(97)\n`,
  );

  const run = () => {
    const r = spawnSync("bash", [join(root, "examples/widgets/git.sh")], {
      cwd: project,
      env: { ...env, OMP_WIDGET_PROJECT: project },
      encoding: "utf8",
      timeout: 30000,
    });
    assert.equal(r.status, 0, `git.sh exit ${r.status}: ${r.stderr}`);
    assert.equal(existsSync(canary), false, "hostile json.py in the project was executed");
    const parsed = parseOutput(r.stdout);
    assert.ok(parsed.ok, `parseOutput rejected git.sh output: ${parsed.error}`);
    assert.equal(parsed.widget.skipped, 0, `skipped blocks in ${r.stdout}`);
    return parsed.widget;
  };
  const kv = (w) => Object.fromEntries(w.blocks.find((b) => b.type === "kv").rows.map((r) => [r.k, r.v]));
  const list = (w) => w.blocks.find((b) => b.type === "list");

  check("no upstream: recent commits labelled as such, project json.py not imported", () => {
    const w = run();
    assert.deepEqual(kv(w), { branch: "main", upstream: "(no upstream)" });
    assert.equal(list(w).title, "recent commits (no upstream)");
    assert.equal(w.blocks.some((b) => b.type === "status"), false);
  });

  check("upstream with one unpushed commit (non-UTF-8 subject): ahead 1, warn", () => {
    spawnSync("git", ["init", "-q", "--bare", upstream], { stdio: "ignore", env });
    git("remote", "add", "origin", upstream);
    git("push", "-q", "-u", "origin", "main");
    writeFileSync(join(project, "g.txt"), "y\n");
    git("add", "g.txt");
    // A raw latin1 subject byte: invalid UTF-8 must not abort the script.
    const r = spawnSync("git", ["-C", project, "commit", "-q", "-F", "-"], { input: Buffer.from("caf\xe9", "latin1"), env });
    assert.equal(r.status, 0, `commit: ${r.stderr}`);
    const w = run();
    assert.equal(w.status, "warn");
    assert.equal(kv(w).upstream, "origin/main");
    assert.equal(w.blocks.find((b) => b.type === "status").value, "1 / 0");
    assert.equal(list(w).title, "unpushed commits");
    assert.equal(list(w).items.length, 1);
  });

  check("detached HEAD: branch row names the commit", () => {
    const sha = git("rev-parse", "--short", "HEAD");
    git("checkout", "-q", "--detach");
    assert.equal(kv(run()).branch, `detached @ ${sha}`);
  });

  console.log(`test-widget-examples: ${passed} checks passed`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
