#!/usr/bin/env node
// Regression script for examples/widgets/git.sh: a widget runs with the
// (untrusted) project as cwd, so a hostile `json.py` in the repo root must
// never be imported by the script's python helper, and the output must pass
// the desktop's own parser (src/app/widgets.js parseOutput) with no block
// skipped — on a branch with an upstream, without one, and on a detached HEAD.
// The project's git config must never get a helper run: clean/process filters
// (status, also a submodule's own) and the signature verifier (log); a filter
// from the user's global config keeps working.
// Run: node tests/test-widget-examples.mjs

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, utimesSync, writeFileSync } from "node:fs";
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
  const gitWith = (extra, dir, ...args) => {
    const r = spawnSync("git", ["-C", dir, ...args], { encoding: "utf8", env: { ...env, ...extra } });
    assert.equal(r.status, 0, `git ${args.join(" ")}: ${r.stderr}`);
    return r.stdout.trim();
  };
  const git = (...args) => gitWith({}, project, ...args);
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

  const run = (dir = project, extra = {}) => {
    const r = spawnSync("bash", [join(root, "examples/widgets/git.sh")], {
      cwd: dir,
      env: { ...env, ...extra, OMP_WIDGET_PROJECT: dir },
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
  const changed = (w) => w.blocks.find((b) => b.type === "number").value;
  // Paths inside shell snippets git runs: forward slashes work on Windows too.
  const fwd = (p) => p.replace(/\\/g, "/");
  // A new mtime makes status re-read (and so clean) a tracked file.
  let stamp = Date.now() / 1000;
  const stale = (dir, ...files) => {
    stamp += 10;
    for (const f of files) utimesSync(join(dir, f), stamp, stamp);
  };
  const lockless = { ...env, GIT_OPTIONAL_LOCKS: "0" };

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

  check("project git config runs no helper: clean/process filters, signature verifier", () => {
    const marks = { clean: join(tmp, "clean-ran"), process: join(tmp, "process-ran"), gpg: join(tmp, "gpg-ran") };
    writeFileSync(join(project, "p.md"), "p\n");
    git("add", "p.md");
    git("commit", "-q", "-m", "p");
    // Re-store HEAD with a signature header, so signature display calls gpg.program.
    const signed = git("cat-file", "commit", "HEAD")
      .replace(/^(committer .*)$/m, "$1\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n AAAA\n -----END PGP SIGNATURE-----") + "\n";
    const hashed = spawnSync("git", ["-C", project, "hash-object", "-t", "commit", "-w", "--stdin"], { input: signed, encoding: "utf8", env });
    assert.equal(hashed.status, 0, `hash-object: ${hashed.stderr}`);
    git("update-ref", "HEAD", hashed.stdout.trim());
    const gpg = join(tmp, "gpg.sh");
    writeFileSync(gpg, `#!/bin/sh\n: > '${fwd(marks.gpg)}'\nexit 1\n`);
    chmodSync(gpg, 0o755);
    // info/attributes outranks every other attributes file: a reader cannot opt out of it.
    writeFileSync(join(project, ".git/info/attributes"), "*.txt filter=evil\n*.md filter=Proc.x\n");
    git("config", "filter.evil.clean", `: > '${fwd(marks.clean)}'; cat`);
    git("config", "filter.evil.required", "true");
    git("config", "filter.Proc.x.process", `: > '${fwd(marks.process)}'`);
    git("config", "log.showSignature", "true");
    git("config", "gpg.program", fwd(gpg));
    // Control: plain git with the same config does run all three helpers here.
    stale(project, "f.txt", "g.txt", "p.md");
    spawnSync("git", ["-C", project, "status", "--porcelain"], { stdio: "ignore", env: lockless });
    spawnSync("git", ["-C", project, "log", "-1"], { stdio: "ignore", env: lockless });
    for (const [name, mark] of Object.entries(marks)) assert.ok(existsSync(mark), `fixture: plain git did not run the ${name} helper`);
    for (const mark of Object.values(marks)) rmSync(mark);
    stale(project, "f.txt", "g.txt", "p.md");
    const w = run();
    for (const [name, mark] of Object.entries(marks)) assert.equal(existsSync(mark), false, `git.sh ran the project's ${name} helper`);
    // Only the untracked json.py: the re-read tracked files still match the index.
    assert.equal(changed(w), 1);
    assert.equal(list(w).items.length, 3);
  });

  check("a filter only the user's global config defines still applies; a project override of it does not run", () => {
    const repo = join(tmp, "lfs-like");
    const global = join(tmp, "global.gitconfig");
    const mark = join(tmp, "up-ran");
    writeFileSync(global, "[filter \"up\"]\n\tclean = tr a-z A-Z\n\tsmudge = cat\n\trequired = true\n");
    const user = { GIT_CONFIG_GLOBAL: global };
    gitWith(user, tmp, "init", "-q", repo);
    writeFileSync(join(repo, ".gitattributes"), "*.txt filter=up\n");
    writeFileSync(join(repo, "a.txt"), "abc\n");
    gitWith(user, repo, "add", ".");
    gitWith(user, repo, "commit", "-q", "-m", "i");
    // The index holds the cleaned "ABC": without the filter a.txt would differ.
    stale(repo, "a.txt");
    assert.equal(changed(run(repo, user)), 0);
    gitWith(user, repo, "config", "filter.up.clean", `: > '${fwd(mark)}'; cat`);
    stale(repo, "a.txt");
    run(repo, user);
    assert.equal(existsSync(mark), false, "git.sh ran the project's override of a global filter");
  });

  check("a submodule's own filter does not run: its files are not checked", () => {
    const sub = join(tmp, "sub");
    const top = join(tmp, "top");
    const mark = join(tmp, "sub-ran");
    gitWith({}, tmp, "init", "-q", sub);
    writeFileSync(join(sub, "a.txt"), "a\n");
    gitWith({}, sub, "add", "a.txt");
    gitWith({}, sub, "commit", "-q", "-m", "s");
    gitWith({}, tmp, "init", "-q", top);
    gitWith({}, top, "-c", "protocol.file.allow=always", "submodule", "add", "-q", fwd(sub), "s");
    gitWith({}, top, "commit", "-q", "-m", "top");
    const checkout = join(top, "s");
    const subGitDir = gitWith({}, checkout, "rev-parse", "--absolute-git-dir");
    mkdirSync(join(subGitDir, "info"), { recursive: true });
    writeFileSync(join(subGitDir, "info", "attributes"), "*.txt filter=sf\n");
    gitWith({}, checkout, "config", "filter.sf.clean", `: > '${fwd(mark)}'; cat`);
    // Control: plain git status checks the submodule's files, under its filter.
    stale(checkout, "a.txt");
    spawnSync("git", ["-C", top, "status", "--porcelain"], { stdio: "ignore", env: lockless });
    assert.ok(existsSync(mark), "fixture: plain git did not run the submodule's filter");
    rmSync(mark);
    stale(checkout, "a.txt");
    assert.equal(changed(run(top)), 0);
    assert.equal(existsSync(mark), false, "git.sh ran the submodule's clean filter");
  });

  console.log(`test-widget-examples: ${passed} checks passed`);
} finally {
  rmSync(tmp, { recursive: true, force: true });
}
