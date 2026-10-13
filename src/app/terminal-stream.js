// Terminal output sync: the pure decisions behind keeping one tab's xterm in
// step with Rust's shell output (app/use-terminal.jsx does the IO).
//
// Rust numbers every spawn with a global, increasing `instance`, and each
// `terminal://data` chunk of an instance with a `seq` (1, 2, …); it keeps
// the latest output in a ring that `terminal_replay(instance, afterSeq)`
// returns. Per tab the hook holds a stream record
//
//   { instance, lastSeq, syncing, pending, exitShown }
//
// `instance` / `lastSeq` = the shell whose output the xterm shows and the
// last seq written; `syncing` = replays running or queued, during which
// events wait in `pending` (arrival order); `exitShown` = the instance whose
// "shell exited" line is printed. Records are immutable: every function
// returns `{ stream, actions }`, the next record and what the hook must do,
// in order:
//
//   { type: "write", bytes, ack: { instance, n } | null }
//       write to the xterm; with `ack`, tell Rust once xterm parsed it
//       (flow control). Replayed bytes are never acked: a replay resets
//       Rust's count.
//   { type: "skipped" }      Rust's ring dropped output: write a marker
//   { type: "exit", instance, code }   panel state: that shell exited
//   { type: "exitLine", code }         write the "shell exited" line
//   { type: "status" }       ask Rust for the tab's status (newer shell)
//   { type: "replay" }       run a replay: `replayCursor(stream)` when it
//       starts, `onReplay` with the reply, then `endSync` exactly once,
//       whatever happened. The action already counts in `syncing`;
//       replays run one after another.
//
// Exposes `window.OMP_TERMINAL_STREAM`; dependency-free IIFE (AGENTS.md
// "IIFE rule"). app/use-terminal.jsx destructures it at top level, so it
// must load before it.
(function () {
  const EMPTY = Object.freeze({ instance: 0, lastSeq: 0, syncing: 0, pending: Object.freeze([]), exitShown: 0 });

  const done = (stream, actions = []) => ({ stream, actions });

  /** Start a replay: queued events wait until it ends. */
  function beginSync(st) {
    return { ...st, syncing: st.syncing + 1 };
  }

  const withReplay = (st, actions) => done(beginSync(st), [...actions, { type: "replay" }]);

  /** The exit line of `instance`, once. */
  function exitLine(st, instance, code, actions) {
    if (st.exitShown === instance) return done(st, actions);
    return done({ ...st, exitShown: instance }, [...actions, { type: "exitLine", code }]);
  }

  /**
   * A `terminal://data` (`{ kind: "data", instance, seq, bytes }`) or
   * `terminal://exit` (`{ kind: "exit", instance, code }`) event.
   * - While a replay runs: queued.
   * - An older instance: dropped.
   * - A newer instance is adopted (its output starts at seq 1) and its
   *   status fetched; an exit of a shell whose output was never seen
   *   replays that output first, then applies.
   * - Data: a seq already written is dropped; a gap replays from `lastSeq`
   *   and re-queues the event (by then covered by the replay, or written
   *   as is — `replayed` keeps it from replaying twice); otherwise written
   *   and acked.
   */
  function onEvent(st, ev) {
    if (st.syncing > 0) return done({ ...st, pending: [...st.pending, ev] });
    if (ev.instance < st.instance) return done(st);
    const actions = [];
    if (ev.instance > st.instance) {
      st = { ...st, instance: ev.instance, lastSeq: 0 };
      actions.push({ type: "status" });
      if (ev.kind === "exit") return withReplay({ ...st, pending: [...st.pending, ev] }, actions);
    }
    if (ev.kind === "exit") {
      actions.push({ type: "exit", instance: ev.instance, code: ev.code });
      return exitLine(st, ev.instance, ev.code, actions);
    }
    if (ev.seq <= st.lastSeq) return done(st, actions);
    if (ev.seq > st.lastSeq + 1 && !ev.replayed) {
      return withReplay({ ...st, pending: [...st.pending, { ...ev, replayed: true }] }, actions);
    }
    actions.push({ type: "write", bytes: ev.bytes, ack: { instance: ev.instance, n: ev.bytes.length } });
    return done({ ...st, lastSeq: ev.seq }, actions);
  }

  /** What a replay starting now asks Rust for. */
  function replayCursor(st) {
    return { instance: st.instance, afterSeq: st.lastSeq };
  }

  /**
   * Rust's reply (`{ instance, bytes, headSeq, dropped } | null`) to a
   * replay after `afterSeq`. A reply for an older shell is ignored; a newer
   * one means Rust replayed its current shell whole, which is adopted. A
   * `dropped` reply after `afterSeq > 0` lost output: marked.
   */
  function onReplay(st, afterSeq, reply) {
    if (!reply || reply.instance < st.instance) return done(st);
    const actions = [];
    if (reply.instance > st.instance) st = { ...st, instance: reply.instance, lastSeq: 0 };
    else if (reply.dropped && afterSeq > 0) actions.push({ type: "skipped" });
    if (reply.bytes.length) actions.push({ type: "write", bytes: reply.bytes, ack: null });
    return done({ ...st, lastSeq: Math.max(st.lastSeq, reply.headSeq) }, actions);
  }

  /**
   * A replay ended (reply applied, or failed). The last one to end runs the
   * queued events in arrival order — one of which may start another
   * replay, queueing the rest again.
   */
  function endSync(st) {
    st = { ...st, syncing: Math.max(0, st.syncing - 1) };
    if (st.syncing > 0 || st.pending.length === 0) return done(st);
    const queued = st.pending;
    st = { ...st, pending: [] };
    const actions = [];
    for (const ev of queued) {
      const r = onEvent(st, ev);
      st = r.stream;
      actions.push(...r.actions);
    }
    return done(st, actions);
  }

  /**
   * First sync of a new xterm, run as a replay (`beginSync` first, `endSync`
   * after): Rust's status (null: no shell yet) and then the whole ring of
   * that instance. An exited shell gets its exit line.
   */
  function onAttach(st, status, reply) {
    if (!status) return done(st);
    st = { ...st, instance: status.instance, lastSeq: 0 };
    if (!reply) return done(st);
    const r = onReplay(st, 0, reply);
    if (r.stream.instance !== status.instance || status.state !== "exited") return r;
    return exitLine(r.stream, status.instance, status.exitCode, r.actions);
  }

  /**
   * `terminal_open` returned `status`: a newer shell is adopted, then a
   * replay picks up what it printed before the reply arrived. A reply older
   * than the shell shown changes nothing.
   */
  function onOpened(st, status) {
    if (status.instance < st.instance) return done(st);
    if (status.instance > st.instance) st = { ...st, instance: status.instance, lastSeq: 0 };
    return withReplay(st, []);
  }

  window.OMP_TERMINAL_STREAM = {
    EMPTY,
    beginSync,
    onEvent,
    replayCursor,
    onReplay,
    endSync,
    onAttach,
    onOpened,
  };
})();
