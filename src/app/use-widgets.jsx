/* app/use-widgets.jsx — loads widgets.json, decides which plugins apply to
   the active tab, and runs them on triggers (activate, turn_end, branch,
   manual). Transitions live in app/widgets-schedule.js; this file only
   wires effects. */

const { OMP_WIDGETS: _UW, OMP_WIDGET_SCHEDULE: _UWS } = window;

function useWidgets({ bridge, activeSessionId, activeProjectPath, branch, streaming, sessionIds, tweaks }) {
  const [config, setConfig] = React.useState({ plugins: [], errors: [], fatal: null, path: "" });
  const [ctx, setCtx]       = React.useState({ project: "", remote: "" });
  const [sched, setSched]   = React.useState(_UWS.EMPTY);
  const schedRef = React.useRef(sched);
  schedRef.current = sched;

  const reload = React.useCallback(async () => {
    if (!bridge) return;
    const [cfg, path] = await Promise.all([bridge.widgetConfig(), bridge.widgetConfigPath()]);
    setConfig({ plugins: cfg.plugins ?? [], errors: cfg.errors ?? [], fatal: cfg.fatal ?? null, path });
  }, [bridge]);
  React.useEffect(() => { reload(); }, [reload]);

  // Context (branch/remote) for the active tab — re-read when the tab, its
  // project or its branch chip changes.
  React.useEffect(() => {
    if (!bridge || !activeSessionId) { setCtx({ project: "", remote: "" }); return; }
    let live = true;
    bridge.widgetContext(activeSessionId, "activate").then(c => { if (live) setCtx(c); });
    return () => { live = false; };
  }, [bridge, activeSessionId, activeProjectPath, branch]);

  const plugins = React.useMemo(
    () => _UW.applicable(config.plugins, ctx).filter(p => (tweaks[_UW.tweakKey(p.id)] ?? true)),
    [config.plugins, ctx, tweaks]);

  // The one place a run is started. `sid` is captured here so a result that
  // lands after a tab switch still goes to the tab it was started for.
  const launch = React.useCallback((sid, plugin, trigger) => {
    const started = _UWS.start(schedRef.current, sid, plugin.id, trigger);
    setSched(started.state); schedRef.current = started.state;
    if (!started.run) return;
    bridge.runWidgetPlugin(plugin.id, sid, trigger).then(res => {
      const parsed = res.error != null ? { ok: false, error: res.error } : _UW.parseOutput(res.stdout ?? "");
      const done = _UWS.finish(schedRef.current, sid, plugin.id, started.seq, { ...parsed, durationMs: res.durationMs });
      setSched(done.state); schedRef.current = done.state;
      if (done.next) launch(sid, plugin, done.next);
    });
  }, [bridge]);

  const fire = React.useCallback((trigger) => {
    if (!activeSessionId) return;
    for (const p of plugins) if (_UW.wantsTrigger(p, trigger)) launch(activeSessionId, p, trigger);
  }, [activeSessionId, plugins, launch]);

  // activate: tab/project changed, or the applicable plugin set changed for it.
  const pluginKey = plugins.map(p => p.id).join("\0");
  React.useEffect(() => { fire("activate"); }, [activeSessionId, activeProjectPath, pluginKey]); // eslint-disable-line react-hooks/exhaustive-deps
  // turn_end: streaming true → false.
  const wasStreaming = React.useRef(streaming);
  React.useEffect(() => {
    if (wasStreaming.current && !streaming) fire("turn_end");
    wasStreaming.current = streaming;
  }, [streaming, fire]);
  // branch: chip changed (skip the initial mount, activate covers it).
  const lastBranch = React.useRef(branch);
  React.useEffect(() => {
    if (lastBranch.current !== branch && lastBranch.current !== undefined) fire("branch");
    lastBranch.current = branch;
  }, [branch, fire]);
  // Closed tabs drop their results.
  React.useEffect(() => { setSched(s => _UWS.prune(s, sessionIds)); }, [sessionIds]);

  const refresh = React.useCallback((pluginId) => {
    const p = plugins.find(x => x.id === pluginId);
    if (p && activeSessionId) launch(activeSessionId, p, "manual");
  }, [plugins, activeSessionId, launch]);

  return {
    plugins, configErrors: config.errors, fatal: config.fatal, configPath: config.path,
    allPlugins: config.plugins,
    runs: sched.runs[activeSessionId] ?? {},
    refresh, reload,
  };
}

window.useWidgets = useWidgets;
