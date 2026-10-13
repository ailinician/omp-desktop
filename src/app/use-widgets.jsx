/* app/use-widgets.jsx — loads widgets.json, decides which plugins apply to
   the active tab, and runs them on triggers (activate, turn_end, branch,
   manual, interval). Transitions live in app/widgets-schedule.js; this file
   only wires effects. */

const { OMP_WIDGETS: _UW, OMP_WIDGET_SCHEDULE: _UWS } = window;

function useWidgets({ bridge, activeSessionId, activeProjectPath, branch, streaming, sessionIds, tweaks }) {
  const [config, setConfig] = React.useState({ plugins: [], errors: [], fatal: null, path: "" });
  // `ctx` carries the session it was read for, so a stale one (tab just
  // switched, widgetContext still in flight) is never matched against the new tab.
  const [ctx, setCtx]       = React.useState({ sessionId: null, project: "", remote: "" });
  const [sched, setSched]   = React.useState(_UWS.EMPTY);
  const schedRef = React.useRef(sched);
  schedRef.current = sched;
  const sessionIdsRef = React.useRef(sessionIds);
  sessionIdsRef.current = sessionIds;
  const activeSessionIdRef = React.useRef(activeSessionId);
  activeSessionIdRef.current = activeSessionId;

  const reload = React.useCallback(async () => {
    if (!bridge) return;
    const [cfg, path] = await Promise.all([bridge.widgetConfig(), bridge.widgetConfigPath()]);
    setConfig({ plugins: cfg.plugins ?? [], errors: cfg.errors ?? [], fatal: cfg.fatal ?? null, path });
  }, [bridge]);
  React.useEffect(() => { reload(); }, [reload]);

  // Context (branch/remote) for the active tab — re-read when the tab, its
  // project or its branch chip changes.
  React.useEffect(() => {
    if (!bridge || !activeSessionId) { setCtx({ sessionId: null, project: "", remote: "" }); return; }
    let live = true;
    bridge.widgetContext(activeSessionId, "activate").then(c => {
      if (live) setCtx({ sessionId: activeSessionId, project: c.project, remote: c.remote });
    });
    return () => { live = false; };
  }, [bridge, activeSessionId, activeProjectPath, branch]);

  const ctxCurrent = ctx.sessionId === activeSessionId;
  const plugins = React.useMemo(
    () => ctxCurrent
      ? _UW.applicable(config.plugins, ctx).filter(p => (tweaks[_UW.tweakKey(p.id)] ?? true))
      : [],
    [config.plugins, ctx, ctxCurrent, tweaks]);

  // The one place a run is started. `sid` is captured here so a result that
  // lands after a tab switch still goes to the tab it was started for; a
  // global plugin's result is keyed by the pseudo-session instead.
  const launch = React.useCallback((sid, plugin, trigger) => {
    const key = _UWS.keyFor(plugin, sid);
    const started = _UWS.start(schedRef.current, key, plugin.id, trigger);
    setSched(started.state); schedRef.current = started.state;
    if (!started.run) return;
    bridge.runWidgetPlugin(plugin.id, sid, trigger).then(res => {
      let parsed;
      try {
        parsed = res.error != null ? { ok: false, error: res.error } : _UW.parseOutput(res.stdout ?? "");
      } catch (err) {
        // finish() must always run, or the plugin's slot stays "running" forever.
        parsed = { ok: false, error: "could not parse plugin output: " + String(err?.message ?? err) };
      }
      const done = _UWS.finish(schedRef.current, key, plugin.id, started.seq, { ...parsed, durationMs: res.durationMs });
      setSched(done.state); schedRef.current = done.state;
      if (!done.next) return;
      const global = key === _UWS.GLOBAL;
      // A tab closed while its run was in flight must not spawn the follow-up;
      // a global follow-up runs for whichever tab is active now.
      const nextSid = global ? activeSessionIdRef.current : sid;
      if (!nextSid || (!global && !sessionIdsRef.current.includes(sid))) return;
      // A queued interval run was due when it was queued: drop it unless the
      // window is visible and (tab plugins) the tab is still the active one.
      if (done.next === "interval" && (document.visibilityState !== "visible" || (!global && sid !== activeSessionIdRef.current))) return;
      launch(nextSid, plugin, done.next);
    });
  }, [bridge]);

  const fire = React.useCallback((trigger) => {
    if (!activeSessionId) return;
    const now = Date.now();
    for (const p of plugins) {
      if (!_UW.wantsTrigger(p, trigger)) continue;
      // A global plugin already has a fresh result when the tab merely changed.
      if (trigger === "activate" && !_UWS.activateDue(p, _UWS.runOf(schedRef.current, _UWS.keyFor(p, activeSessionId), p.id), now)) continue;
      launch(activeSessionId, p, trigger);
    }
  }, [activeSessionId, plugins, launch]);

  // activate: tab/project changed, or the applicable plugin set changed for it.
  // Plugins are empty until this tab's context has arrived, so the effect
  // re-runs (pluginKey changes) exactly once with the right plugin set.
  const pluginKey = plugins.map(p => p.id + ":" + (p.intervalMs ?? 0)).join("\0");
  React.useEffect(() => {
    if (ctxCurrent) fire("activate");
  }, [activeSessionId, activeProjectPath, pluginKey]); // eslint-disable-line react-hooks/exhaustive-deps
  // turn_end: streaming true → false within the same tab (a tab switch is not a turn end).
  const wasStreaming = React.useRef({ sessionId: activeSessionId, streaming });
  React.useEffect(() => {
    if (_UWS.turnEnded(wasStreaming.current, { sessionId: activeSessionId, streaming })) fire("turn_end");
    wasStreaming.current = { sessionId: activeSessionId, streaming };
  }, [activeSessionId, streaming, fire]);
  // branch: chip changed (skip the initial mount, activate covers it).
  const lastBranch = React.useRef(branch);
  React.useEffect(() => {
    if (lastBranch.current !== branch && lastBranch.current !== undefined) fire("branch");
    lastBranch.current = branch;
  }, [branch, fire]);
  // Closed tabs drop their results. Goes through schedRef like start/finish
  // so a result landing in the same tick sees the pruned state.
  React.useEffect(() => {
    const next = _UWS.prune(schedRef.current, sessionIds);
    schedRef.current = next;
    setSched(next);
  }, [sessionIds]);

  const refresh = React.useCallback((pluginId) => {
    const p = plugins.find(x => x.id === pluginId);
    if (p && activeSessionId) launch(activeSessionId, p, "manual");
  }, [plugins, activeSessionId, launch]);

  // Interval timers + visibility catch-up for `list`. Paused while the window
  // is hidden; on return to visible each due plugin fires once and its timer
  // restarts, so the next tick cannot queue a second run right behind it.
  const startIntervals = React.useCallback((list) => {
    const timers = new Map();
    const runFor = (p) => {
      const sid = activeSessionIdRef.current;
      if (sid) launch(sid, p, "interval");
    };
    const arm = (p) => {
      clearInterval(timers.get(p.id));
      timers.set(p.id, setInterval(() => { if (document.visibilityState === "visible") runFor(p); }, p.intervalMs));
    };
    list.forEach(arm);
    const onVisible = () => {
      if (document.visibilityState !== "visible") return;
      const now = Date.now();
      for (const p of list) {
        const run = _UWS.runOf(schedRef.current, _UWS.keyFor(p, activeSessionIdRef.current), p.id);
        if (!_UWS.dueNow(run, p.intervalMs, now)) continue;
        runFor(p);
        arm(p);
      }
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => {
      for (const t of timers.values()) clearInterval(t);
      document.removeEventListener("visibilitychange", onVisible);
    };
  }, [launch]);

  // Tab plugins: active tab only; timers restart on a tab switch or when the
  // plugin set or any intervalMs changes (pluginKey carries the intervals).
  React.useEffect(
    () => startIntervals(plugins.filter(p => p.intervalMs && p.scope !== "global")),
    [activeSessionId, pluginKey, startIntervals]); // eslint-disable-line react-hooks/exhaustive-deps
  // Global plugins: one timer set for the app, untouched by a tab switch.
  const globalKey = plugins.filter(p => p.intervalMs && p.scope === "global").map(p => p.id + ":" + p.intervalMs).join("\0");
  React.useEffect(
    () => startIntervals(plugins.filter(p => p.intervalMs && p.scope === "global")),
    [globalKey, startIntervals]); // eslint-disable-line react-hooks/exhaustive-deps

  return {
    plugins, configErrors: config.errors, fatal: config.fatal, configPath: config.path,
    allPlugins: config.plugins,
    runs: { ...sched.runs[_UWS.GLOBAL], ...sched.runs[activeSessionId] },
    refresh, reload,
  };
}

window.useWidgets = useWidgets;
