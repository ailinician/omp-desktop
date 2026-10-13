#!/usr/bin/env node
// Minimal omp-desktop rail widget in Node: echoes the context it was given.
const ctx = JSON.parse(await new Promise(r => { let s = ""; process.stdin.on("data", d => s += d).on("end", () => r(s || "{}")); }));
const rows = ["project", "branch", "remote", "trigger"].map(k => ({ k, v: String(ctx[k] ?? "") }));
console.log(JSON.stringify({ version: 1, status: "ok", blocks: [{ type: "kv", rows }] }));
