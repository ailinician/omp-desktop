#!/usr/bin/env node
// Regression script for src/app/terminal-stream.js — the terminal's output
// sync decisions: instance adoption, seq dedupe, gap → replay, the event
// queue while a replay runs, which writes are acked, exit lines, and the
// attach / open replays. Also drives the reducer against a fake Rust ring
// the way app/use-terminal.jsx does, to check the xterm ends up with every
// byte exactly once and in order.
// Run: node tests/test-terminal-stream.mjs

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const win = {};
// eslint-disable-next-line no-new-func
new Function("window", readFileSync(join(root, "src/app/terminal-stream.js"), "utf8"))(win);
const S = win.OMP_TERMINAL_STREAM;

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

const enc = (s) => new TextEncoder().encode(s);
const data = (instance, seq, text = `<${instance}.${seq}>`) => ({ kind: "data", instance, seq, bytes: enc(text) });
const exit = (instance, code = 0) => ({ kind: "exit", instance, code });
const reply = (instance, text, headSeq, dropped = false) => ({ instance, bytes: enc(text), headSeq, dropped });
const types = (r) => r.actions.map((a) => a.type);
const at = (instance, lastSeq, over = {}) => ({ ...S.EMPTY, instance, lastSeq, ...over });

// ---- live data --------------------------------------------------------------

check("EMPTY shows no shell and is frozen", () => {
  assert.deepEqual({ ...S.EMPTY, pending: [...S.EMPTY.pending] },
    { instance: 0, lastSeq: 0, syncing: 0, pending: [], exitShown: 0 });
  assert.ok(Object.isFrozen(S.EMPTY) && Object.isFrozen(S.EMPTY.pending));
});

check("the next seq is written and acked with its byte count", () => {
  const ev = data(1, 3, "hello");
  const r = S.onEvent(at(1, 2), ev);
  assert.deepEqual(r.actions, [{ type: "write", bytes: ev.bytes, ack: { instance: 1, n: 5 } }]);
  assert.equal(r.stream.lastSeq, 3);
});

check("a seq already written and an older instance are dropped", () => {
  const st = at(2, 5);
  for (const ev of [data(2, 5), data(2, 1), data(1, 9), exit(1)]) {
    const r = S.onEvent(st, ev);
    assert.equal(r.stream, st, `${ev.kind} ${ev.instance}.${ev.seq}`);
    assert.deepEqual(r.actions, []);
  }
});

check("inputs are never mutated", () => {
  const st = Object.freeze(at(1, 1, { pending: Object.freeze([]) }));
  S.onEvent(st, data(1, 2));
  S.onEvent(st, data(1, 9));
  S.endSync(S.beginSync(st));
});

// ---- gaps and the queue -------------------------------------------------------

check("a gap queues the event (marked replayed) and asks for a replay", () => {
  const ev = data(1, 7);
  const r = S.onEvent(at(1, 4), ev);
  assert.deepEqual(types(r), ["replay"]);
  assert.equal(r.stream.syncing, 1, "the replay action is already counted");
  assert.deepEqual(r.stream.pending, [{ ...ev, replayed: true }]);
  assert.deepEqual(S.replayCursor(r.stream), { instance: 1, afterSeq: 4 });
});

check("events during a replay wait and run in arrival order once it ends", () => {
  let st = S.beginSync(at(1, 0));
  const evs = [data(1, 1, "a"), data(1, 2, "b"), exit(1, 3)];
  for (const ev of evs) {
    const r = S.onEvent(st, ev);
    assert.deepEqual(r.actions, [], "nothing runs while syncing");
    st = r.stream;
  }
  assert.equal(st.pending.length, 3);
  const r = S.endSync(st);
  assert.deepEqual(types(r), ["write", "write", "exit", "exitLine"]);
  assert.equal(r.stream.pending.length, 0);
  assert.equal(r.stream.syncing, 0);
});

check("the queue drains only when the last replay ends", () => {
  let st = S.beginSync(S.beginSync(at(1, 0)));
  st = S.onEvent(st, data(1, 1)).stream;
  const first = S.endSync(st);
  assert.deepEqual(first.actions, []);
  assert.equal(first.stream.syncing, 1);
  assert.equal(first.stream.pending.length, 1);
  assert.deepEqual(types(S.endSync(first.stream)), ["write"]);
});

check("a gap while draining replays again and re-queues the rest behind it", () => {
  let st = S.beginSync(at(1, 1));
  for (const ev of [data(1, 5), data(1, 6)]) st = S.onEvent(st, ev).stream;
  const r = S.endSync(st);
  assert.deepEqual(types(r), ["replay"]);
  assert.equal(r.stream.syncing, 1);
  assert.deepEqual(r.stream.pending.map((e) => [e.seq, !!e.replayed]), [[5, true], [6, false]]);
});

check("a replayed event the replay covered is dropped; one it did not is written as is", () => {
  const covered = S.onEvent(at(1, 9), { ...data(1, 7), replayed: true });
  assert.deepEqual(covered.actions, []);
  const beyond = S.onEvent(at(1, 4), { ...data(1, 7), replayed: true });
  assert.deepEqual(types(beyond), ["write"], "no second replay for the same event");
  assert.equal(beyond.stream.lastSeq, 7);
});

check("endSync never counts below zero", () => {
  assert.equal(S.endSync(S.EMPTY).stream.syncing, 0);
});

// ---- replies ------------------------------------------------------------------

check("a replay reply is written unacked and moves lastSeq to its head", () => {
  const r = S.onReplay(at(1, 4), 4, reply(1, "xyz", 6));
  assert.deepEqual(r.actions, [{ type: "write", bytes: enc("xyz"), ack: null }]);
  assert.equal(r.stream.lastSeq, 6);
});

check("a reply never moves lastSeq back, and empty bytes write nothing", () => {
  const r = S.onReplay(at(1, 8), 4, reply(1, "", 6));
  assert.deepEqual(r.actions, []);
  assert.equal(r.stream.lastSeq, 8);
});

check("dropped output is marked only when output was already shown", () => {
  assert.deepEqual(types(S.onReplay(at(1, 4), 4, reply(1, "z", 9, true))), ["skipped", "write"]);
  assert.deepEqual(types(S.onReplay(at(1, 0), 0, reply(1, "z", 9, true))), ["write"]);
});

check("a null reply or one for an older shell changes nothing", () => {
  const st = at(3, 2);
  assert.equal(S.onReplay(st, 2, null).stream, st);
  assert.equal(S.onReplay(st, 2, reply(2, "old", 9)).stream, st);
});

check("a reply for a newer shell is its whole output: adopted", () => {
  const r = S.onReplay(at(1, 5), 5, reply(2, "new", 3, true));
  assert.deepEqual(types(r), ["write"], "no skipped marker: a different shell");
  assert.equal(r.stream.instance, 2);
  assert.equal(r.stream.lastSeq, 3);
});

// ---- instances and exits ------------------------------------------------------

check("data of a newer shell is adopted from seq 1, with a status refresh", () => {
  const r = S.onEvent(at(1, 40), data(2, 1));
  assert.deepEqual(types(r), ["status", "write"]);
  assert.equal(r.stream.instance, 2);
  assert.equal(r.stream.lastSeq, 1);
});

check("data of a newer shell that skipped its start replays it", () => {
  const r = S.onEvent(at(1, 40), data(2, 3));
  assert.deepEqual(types(r), ["status", "replay"]);
  assert.deepEqual(S.replayCursor(r.stream), { instance: 2, afterSeq: 0 });
});

check("an exit of the shown shell: panel state and one exit line", () => {
  const r = S.onEvent(at(1, 3), exit(1, 130));
  assert.deepEqual(r.actions, [{ type: "exit", instance: 1, code: 130 }, { type: "exitLine", code: 130 }]);
  assert.equal(r.stream.exitShown, 1);
  assert.deepEqual(types(S.onEvent(r.stream, exit(1, 130))), ["exit"], "no second line");
});

check("an exit of a shell never seen replays its output first", () => {
  const r = S.onEvent(at(1, 3), exit(2, 1));
  assert.deepEqual(types(r), ["status", "replay"]);
  assert.deepEqual(r.stream.pending, [exit(2, 1)]);
  let st = S.onReplay(r.stream, 0, reply(2, "bye", 2)).stream;
  const end = S.endSync(st);
  assert.deepEqual(types(end), ["exit", "exitLine"]);
  st = end.stream;
  assert.equal(st.instance, 2);
  assert.equal(st.exitShown, 2);
});

// ---- attach / open ------------------------------------------------------------

const St = (instance, state = "running", exitCode = null) => ({ instance, shell: "/bin/sh", state, exitCode });

check("attach without a shell changes nothing", () => {
  const st = S.beginSync(S.EMPTY);
  assert.equal(S.onAttach(st, null, null).stream, st);
});

check("attach writes the ring of the running shell", () => {
  const r = S.onAttach(S.beginSync(S.EMPTY), St(4), reply(4, "$ ", 12));
  assert.deepEqual(types(r), ["write"]);
  assert.equal(r.stream.instance, 4);
  assert.equal(r.stream.lastSeq, 12);
  assert.equal(r.stream.syncing, 1, "endSync is the hook's");
});

check("attach to an exited shell adds its exit line once", () => {
  const r = S.onAttach(S.EMPTY, St(4, "exited", 2), reply(4, "out", 3));
  assert.deepEqual(r.actions.slice(1), [{ type: "exitLine", code: 2 }]);
  assert.equal(r.stream.exitShown, 4);
  assert.deepEqual(types(S.onEvent(r.stream, exit(4, 2))), ["exit"], "the late exit event adds no line");
});

check("attach: a newer shell replayed than the exited one reported gets no exit line", () => {
  const r = S.onAttach(S.EMPTY, St(4, "exited", 0), reply(5, "new", 1));
  assert.deepEqual(types(r), ["write"]);
  assert.equal(r.stream.instance, 5);
});

check("attach whose replay failed still shows the status's shell", () => {
  const r = S.onAttach(S.EMPTY, St(4), null);
  assert.deepEqual(r.actions, []);
  assert.equal(r.stream.instance, 4);
});

check("an open reply adopts a newer shell and replays what it printed", () => {
  const r = S.onOpened(at(1, 9, { exitShown: 1 }), St(2));
  assert.deepEqual(types(r), ["replay"]);
  assert.equal(r.stream.syncing, 1);
  assert.deepEqual(S.replayCursor(r.stream), { instance: 2, afterSeq: 0 });
  const same = S.onOpened(at(2, 4), St(2));
  assert.deepEqual(S.replayCursor(same.stream), { instance: 2, afterSeq: 4 }, "a live shell returned as is");
  const old = S.onOpened(at(3, 1), St(2));
  assert.deepEqual(old.actions, []);
  assert.equal(old.stream.syncing, 0);
});

// ---- end to end against a fake Rust ring --------------------------------------

/**
 * Runs the reducer the way the hook does: `replay` actions go on a FIFO
 * chain served from `ring` (instance → chunks), `write`s are collected.
 * Events in `lost` never reach the webview; after event `i`,
 * `interleave(i)` queued replays are answered (the rest at the end).
 */
function simulate({ ring, events, lost = new Set(), interleave = () => Infinity }) {
  let st = S.EMPTY;
  const written = [];
  const acks = [];
  let queued = 0;
  const run = (r) => {
    st = r.stream;
    for (const a of r.actions) {
      if (a.type === "write") {
        written.push(new TextDecoder().decode(a.bytes));
        if (a.ack) acks.push(a.ack);
      } else if (a.type === "replay") queued++;
    }
  };
  const serve = () => {
    queued--;
    const { instance, afterSeq } = S.replayCursor(st);
    const chunks = ring[instance] ?? [];
    const after = chunks.filter((c) => c.seq > afterSeq);
    run(S.onReplay(st, afterSeq, {
      instance, headSeq: chunks.length ? chunks[chunks.length - 1].seq : 0, dropped: false,
      bytes: enc(after.map((c) => c.text).join("")),
    }));
    run(S.endSync(st));
  };
  events.forEach((ev, i) => {
    if (!lost.has(i)) run(S.onEvent(st, ev));
    for (let n = interleave(i); n > 0 && queued > 0; n--) serve();
  });
  while (queued > 0) serve();
  return { text: written.join(""), acks, st };
}

check("every byte exactly once and in order, whatever is lost or delayed", () => {
  const chunks = Array.from({ length: 12 }, (_, i) => ({ seq: i + 1, text: `[${i + 1}]` }));
  const ring = { 1: chunks };
  const events = chunks.map((c) => ({ kind: "data", instance: 1, seq: c.seq, bytes: enc(c.text) }));
  const want = chunks.map((c) => c.text).join("");
  for (const lost of [new Set(), new Set([0]), new Set([3, 4]), new Set([2, 7, 8, 11])]) {
    // lag 0: a replay answers before the next event; lag k: one every k events.
    for (const lag of [0, 1, 3]) {
      const interleave = (i) => (lag === 0 ? Infinity : Number(i % lag === lag - 1));
      const { text, acks, st } = simulate({ ring, events, lost, interleave });
      assert.equal(text, want, `lost ${[...lost]} lag ${lag}`);
      assert.equal(st.lastSeq, 12);
      assert.equal(st.syncing, 0);
      assert.ok(acks.every((a) => a.instance === 1));
      assert.ok(acks.reduce((n, a) => n + a.n, 0) <= enc(want).length, "only live writes are acked, each once");
    }
  }
});

check("a respawn mid-stream: the old shell's late chunks never reach the new one", () => {
  const ring = { 1: [{ seq: 1, text: "a" }, { seq: 2, text: "b" }], 2: [{ seq: 1, text: "X" }, { seq: 2, text: "Y" }] };
  const events = [data(1, 1, "a"), data(2, 1, "X"), data(1, 2, "b"), data(2, 2, "Y")];
  const { text, st } = simulate({ ring, events });
  assert.equal(text, "aXY");
  assert.equal(st.instance, 2);
});

console.log(`terminal-stream: ${passed} checks passed`);
