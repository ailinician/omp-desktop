// app/widgets-schedule.js — pure per-tab run bookkeeping for rail widget
// plugins: one in-flight run per (session, plugin), one coalesced follow-up,
// stale results dropped by sequence number. The hook (app/use-widgets.jsx)
// owns the side effects; this file owns every state transition so
// tests/test-widgets.mjs can pin them.
(function () {
  const EMPTY = Object.freeze({ runs: {} });

  const runOf = (s, sid, pid) => s.runs[sid]?.[pid];
  const put = (s, sid, pid, run) => ({ runs: { ...s.runs, [sid]: { ...(s.runs[sid] ?? {}), [pid]: run } } });

  function start(s, sid, pid, trigger) {
    const prev = runOf(s, sid, pid);
    if (prev?.state === "running") {
      return { state: put(s, sid, pid, { ...prev, queued: trigger }), seq: prev.seq, run: false };
    }
    const seq = (prev?.seq ?? 0) + 1;
    // Keep the last widget so the card can render it dimmed while running.
    const run = { state: "running", seq, widget: prev?.widget, lastOkAt: prev?.lastOkAt, startedTrigger: trigger };
    return { state: put(s, sid, pid, run), seq, run: true };
  }

  function finish(s, sid, pid, seq, result) {
    const prev = runOf(s, sid, pid);
    if (!prev || prev.seq !== seq || prev.state !== "running") return { state: s, next: null };
    const at = result.at ?? Date.now();
    const base = { seq, durationMs: result.durationMs ?? 0, at };
    const run = result.ok
      ? { ...base, state: "ok", widget: result.widget, lastOkAt: at }
      : { ...base, state: "error", error: result.error, lastOkAt: prev.lastOkAt };
    return { state: put(s, sid, pid, run), next: prev.queued ?? null };
  }

  // Global-scope plugins keep one result for all tabs under this pseudo-session.
  const GLOBAL = "__global__";
  const keyFor = (plugin, sid) => (plugin?.scope === "global" ? GLOBAL : sid);

  function prune(s, sessionIds) {
    const keep = new Set([...sessionIds, GLOBAL]);
    const runs = Object.fromEntries(Object.entries(s.runs).filter(([sid]) => keep.has(sid)));
    return { runs };
  }

  // turn_end only counts inside one tab: streaming true → false across a tab switch is not a turn end.
  const turnEnded = (prev, next) => prev.sessionId === next.sessionId && prev.streaming && !next.streaming;

  // dueNow: true when a plugin has never completed a run and none is in flight,
  // or its last run is older than its interval. A run in flight counts as fresh.
  const dueNow = (run, intervalMs, now) => run?.state === "running" ? false : !run?.at || now - run.at >= intervalMs;

  // activateDue: a tab plugin always runs on activate; a global plugin only when
  // it has no result yet or its last one is older than intervalMs (no intervalMs → always).
  const activateDue = (plugin, run, now) => plugin?.scope !== "global" || !plugin.intervalMs || dueNow(run, plugin.intervalMs, now);

  window.OMP_WIDGET_SCHEDULE = { EMPTY, GLOBAL, start, finish, prune, runOf, keyFor, turnEnded, dueNow, activateDue };
})();
