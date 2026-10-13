/* ═════════════════════════════════════════════════════════════════════
   app/use-terminal.jsx — per-tab terminal view for app-live.jsx.
   Rust owns each tab's shell (folder, program, lifetime, output ring);
   this hook renders it. One xterm `Terminal` per tab, kept in a Map ref
   and mounted into its panel's node (design/terminal-panel.jsx); an
   instance stays mounted until its tab goes. Panel state (open / created
   / Rust's status) is the pure reducer in app/terminal.js.

   Output sync: every data event carries the shell's `instance` and a
   per-instance `seq` (1, 2, …). The hook subscribes, waits until both
   listeners are registered, asks Rust for the status and replays the
   ring, so nothing emitted before the subscription is lost. Events that
   arrive while a replay is in flight are queued and run after it; a seq
   gap triggers a replay from the last seq seen. Live chunks are acked
   to Rust once xterm has parsed them (flow control); replayed bytes are
   not (a replay resets Rust's count).

   View policy stays here: the shell starts the first time a tab's panel
   is shown with no shell yet, at the fitted size; Enter in an exited
   (or failed) shell starts a new one; auto-open, height and focus.

   Returns `{ state, toggle, close, respawn, setHeight, mountFor, focus,
   copySelection, shellSettings, shellError, saveShell }` (the last three
   back the Tweaks shell field). `tweaks.terminalHeight` is the panel
   height every panel shares. Every panel body is watched with a
   ResizeObserver: sidebar, tree, subagent pane and window changes all
   end in a (debounced) refit.
   ═════════════════════════════════════════════════════════════════════ */

const {
  EMPTY: _UT_EMPTY, DEFAULT_HEIGHT: _UT_DEFAULT_HEIGHT, openPanel: _UT_open, closePanel: _UT_close, togglePanel: _UT_toggle,
  applyStatus: _UT_applyStatus, applyExit: _UT_applyExit, spawnFailed: _UT_spawnFailed,
  needsSpawn: _UT_needsSpawn, isExited: _UT_isExited, prune: _UT_prune, entryOf: _UT_entryOf,
  keymapShouldBypass: _UT_bypass,
} = window.OMP_TERMINAL;

const _UT_FONT_PX = 13;

/** xterm colours from the active theme's CSS variables. */
function _UT_theme() {
  const css = getComputedStyle(document.documentElement);
  const v = (name, fallback) => css.getPropertyValue(name).trim() || fallback;
  return {
    background: v("--bg-window", "#0B0D14"),
    foreground: v("--fg", "#E6E9F2"),
    cursor: v("--accent", "#8AF0C8"),
    selectionBackground: v("--accent-soft", "rgba(138, 240, 200, 0.25)"),
  };
}

function _UT_exitLine(code) {
  const label = code == null ? "" : ` (code ${code})`;
  return `\r\n\x1b[2mshell exited${label} \u2014 press Enter to restart\x1b[0m\r\n`;
}

function useTerminal({ bridge, activeSessionId, sessionIds, tweaks, setTweak }) {
  const [state, setState] = React.useState(_UT_EMPTY);
  const stateRef = React.useRef(state);
  stateRef.current = state;
  // Bumped when a tab's initial sync settles, so the spawn effect re-checks.
  const [syncTick, setSyncTick] = React.useState(0);

  // sid → rec: { term, fit, mounted, disposed, subs, instance, lastSeq,
  // syncing, sync, pending, exitShown, spawning, observer, fitTimer }.
  // `instance` / `lastSeq` = the shell whose output the xterm shows and its
  // last seq written; `syncing` counts replays queued on the `sync` chain,
  // during which events wait in `pending`; `exitShown` = the instance whose
  // "shell exited" line is printed. sid → stable ref callback for the
  // panel's mount node.
  const recs = React.useRef(new Map());
  const mountRefs = React.useRef(new Map());

  const fontSize = Math.round((_UT_FONT_PX * (tweaks.fontSize ?? 100)) / 100);
  // Read by `create`, whose ref callback outlives the render that made it.
  const fontSizeRef = React.useRef(fontSize);
  fontSizeRef.current = fontSize;
  const height = tweaks.terminalHeight ?? _UT_DEFAULT_HEIGHT;

  const fit = React.useCallback((sid) => {
    const rec = recs.current.get(sid);
    if (!rec?.mounted) return;
    try {
      rec.fit.fit();
      // Output written while the panel was hidden is parsed but not painted.
      rec.term.refresh(0, rec.term.rows - 1);
    } catch { /* hidden or detached: refit on reveal */ }
  }, []);

  /** Ask Rust for the tab's status (a shell we have not been told about). */
  const refreshStatus = React.useCallback(async (sid) => {
    const status = await bridge.terminalStatus(sid);
    setState((s) => _UT_applyStatus(s, sid, status));
  }, [bridge]);

  // Event handling and replays reference each other (a gap replays, a
  // replay flushes queued events): plain functions over refs, rebuilt with
  // `bridge`, reached through `ops.current` by long-lived callbacks.
  const ops = React.useRef(null);
  ops.current = React.useMemo(() => {
    /** Run `step` (async) on the rec's sync chain; events queue until the
     *  chain drains, then run in arrival order. */
    function enqueue(sid, rec, step) {
      rec.syncing += 1;
      rec.sync = rec.sync
        .then(() => (rec.disposed ? undefined : step()))
        .catch((err) => console.error("[terminal] sync failed:", err))
        .finally(() => {
          rec.syncing -= 1;
          if (rec.syncing > 0 || rec.disposed) return;
          const queued = rec.pending;
          rec.pending = [];
          for (const ev of queued) handle(sid, rec, ev);
        });
    }

    /** Write Rust's held output after the rec's `lastSeq` (unacked). */
    function catchUp(sid, rec) {
      enqueue(sid, rec, async () => {
        const after = rec.lastSeq;
        const r = await bridge.terminalReplay(sid, rec.instance, after);
        if (rec.disposed || !r || r.instance < rec.instance) return;
        if (r.instance > rec.instance) {
          // Asked for an older shell: Rust replayed the current one whole.
          rec.instance = r.instance;
          rec.lastSeq = 0;
        } else if (r.dropped && after > 0) {
          rec.term.write("\r\n\x1b[2m[output skipped]\x1b[0m\r\n");
        }
        if (r.bytes.length) rec.term.write(r.bytes);
        rec.lastSeq = Math.max(rec.lastSeq, r.headSeq);
      });
    }

    /** A newer shell than the one shown: its output starts at seq 1. */
    function adopt(sid, rec, instance) {
      rec.instance = instance;
      rec.lastSeq = 0;
      void refreshStatus(sid);
    }

    function handle(sid, rec, ev) {
      if (rec.disposed) return;
      if (rec.syncing > 0) { rec.pending.push(ev); return; }
      if (ev.instance < rec.instance) return;
      if (ev.instance > rec.instance) {
        adopt(sid, rec, ev.instance);
        // An exit with no output seen: whatever it printed is in the ring.
        if (ev.kind === "exit") { rec.pending.push(ev); catchUp(sid, rec); return; }
      }
      if (ev.kind === "exit") {
        setState((s) => _UT_applyExit(s, sid, ev.instance, ev.code));
        if (_UT_entryOf(stateRef.current, sid).status?.instance !== ev.instance) void refreshStatus(sid);
        if (rec.exitShown !== ev.instance) rec.term.write(_UT_exitLine(ev.code));
        rec.exitShown = ev.instance;
        return;
      }
      if (ev.seq <= rec.lastSeq) return;
      if (ev.seq > rec.lastSeq + 1 && !ev.replayed) {
        // Missed chunks: fetch them, then this event runs again from the
        // queue (by then covered by the replay, or written as is).
        rec.pending.push({ ...ev, replayed: true });
        catchUp(sid, rec);
        return;
      }
      rec.lastSeq = ev.seq;
      const { instance, bytes } = ev;
      rec.term.write(bytes, () => bridge.terminalAck(sid, instance, bytes.length));
    }

    /** First sync of a new xterm: listeners first, then status, then replay. */
    function attach(sid, rec) {
      const data = bridge.onTerminalData(sid, (p) => handle(sid, rec, { kind: "data", ...p }));
      const exit = bridge.onTerminalExit(sid, (p) => handle(sid, rec, { kind: "exit", ...p }));
      rec.subs = [data, exit];
      enqueue(sid, rec, async () => {
        await Promise.all([data.ready, exit.ready]);
        if (rec.disposed) return;
        const status = await bridge.terminalStatus(sid);
        if (rec.disposed) return;
        setState((s) => _UT_applyStatus(s, sid, status));
        if (!status) return;
        rec.instance = status.instance;
        rec.lastSeq = 0;
        const r = await bridge.terminalReplay(sid, status.instance, 0);
        if (rec.disposed || !r) return;
        rec.instance = r.instance;
        if (r.bytes.length) rec.term.write(r.bytes);
        rec.lastSeq = r.headSeq;
        if (r.instance === status.instance && status.state === "exited") {
          rec.term.write(_UT_exitLine(status.exitCode));
          rec.exitShown = status.instance;
        }
      });
      rec.sync = rec.sync.finally(() => { if (!rec.disposed) setSyncTick((n) => n + 1); });
    }

    /** Start the tab's shell (Rust returns a live one as is), then fetch
     *  whatever it printed before the reply. */
    async function spawn(sid) {
      const rec = recs.current.get(sid);
      if (!rec || rec.spawning || rec.disposed) return;
      rec.spawning = true;
      const res = await bridge.terminalOpen(sid, rec.term.cols, rec.term.rows);
      rec.spawning = false;
      if (rec.disposed) return;
      if (!res.ok) {
        // Shown as an exited shell: Enter tries again.
        rec.term.write(`\r\n\x1b[31mcould not start the shell: ${res.error}\x1b[0m \u2014 press Enter to retry\r\n`);
        setState((s) => _UT_spawnFailed(s, sid, res.error));
        return;
      }
      const { status } = res;
      setState((s) => _UT_applyStatus(s, sid, status));
      if (status.instance < rec.instance) return;
      if (status.instance > rec.instance) {
        rec.instance = status.instance;
        rec.lastSeq = 0;
      }
      catchUp(sid, rec);
    }

    return { attach, spawn };
  }, [bridge, refreshStatus]);

  const spawn = React.useCallback((sid) => ops.current.spawn(sid), []);

  const respawn = React.useCallback((sid) => {
    const rec = recs.current.get(sid);
    if (!rec || rec.spawning) return;
    rec.term.reset();
    spawn(sid);
  }, [spawn]);

  const create = React.useCallback((sid) => {
    const term = new window.Terminal({
      scrollback: 5000, fontSize: fontSizeRef.current, theme: _UT_theme(), allowProposedApi: false, cursorBlink: true,
    });
    // OSC 8 hyperlinks are on by default in xterm 6, and every one a program
    // prints keeps an invisible link entry for as long as its cells live —
    // unbounded for a shell that streams them. No links addon is loaded, so
    // nothing could use them: swallow the sequence before any output arrives.
    term.parser.registerOscHandler(8, () => true);
    const fitAddon = new window.FitAddon.FitAddon();
    term.loadAddon(fitAddon);
    // `false` hands the key back to the page (the global keymap); keyup/press
    // stay xterm's. Enter in an exited shell restarts it.
    term.attachCustomKeyEventHandler((e) => {
      if (e.type !== "keydown") return true;
      if (e.key === "Enter" && _UT_isExited(_UT_entryOf(stateRef.current, sid))) {
        respawn(sid);
        return false;
      }
      return !_UT_bypass(e);
    });
    term.onData((data) => bridge.terminalWrite(sid, data));
    term.onResize(({ cols, rows }) => bridge.terminalResize(sid, cols, rows));
    const rec = {
      term, fit: fitAddon, mounted: false, disposed: false, subs: [],
      instance: 0, lastSeq: 0, syncing: 0, sync: Promise.resolve(), pending: [], exitShown: 0,
      spawning: false, observer: null, fitTimer: 0,
    };
    recs.current.set(sid, rec);
    ops.current.attach(sid, rec);
    return rec;
  }, [bridge, respawn]);

  const dispose = React.useCallback((sid) => {
    const rec = recs.current.get(sid);
    if (rec) {
      rec.disposed = true;
      for (const sub of rec.subs) sub.stop();
      rec.observer?.disconnect();
      clearTimeout(rec.fitTimer);
      rec.term.dispose();
    }
    recs.current.delete(sid);
    mountRefs.current.delete(sid);
  }, []);

  // The panel's mount node: the first time it appears the tab's xterm opens
  // into it. (The shell itself is spawned by the effect below, once visible.)
  const mountFor = React.useCallback((sid) => {
    if (!mountRefs.current.has(sid)) {
      mountRefs.current.set(sid, (node) => {
        if (!node || !bridge) return;
        const rec = recs.current.get(sid) ?? create(sid);
        if (rec.mounted) return;
        rec.term.open(node);
        rec.mounted = true;
        // Layout changes that resize the body without a window resize
        // (sidebar, tree, subagent pane) still refit. A hidden panel reports
        // 0×0: nothing to fit, the reveal observes the real size.
        rec.observer = new ResizeObserver((entries) => {
          const { width, height: h } = entries[entries.length - 1].contentRect;
          if (width === 0 || h === 0) return;
          clearTimeout(rec.fitTimer);
          rec.fitTimer = setTimeout(() => fit(sid), 50);
        });
        rec.observer.observe(node);
      });
    }
    return mountRefs.current.get(sid);
  }, [bridge, create, fit]);

  /** Right-click on a selection copies it; true when it did (no menu then). */
  const copySelection = React.useCallback((sid) => {
    const term = recs.current.get(sid)?.term;
    if (!term?.hasSelection()) return false;
    window.copyText(term.getSelection());
    return true;
  }, []);

  const focus = React.useCallback((sid) => recs.current.get(sid)?.term.focus(), []);
  const toggle = React.useCallback((sid) => {
    if (!sid) return;
    setState((s) => _UT_toggle(s, sid));
  }, []);
  const close = React.useCallback((sid) => setState((s) => _UT_close(s, sid)), []);
  const setHeight = React.useCallback((px) => setTweak("terminalHeight", px), [setTweak]);

  // A visible panel: size the xterm to it, start the shell of a tab that has
  // none (once its initial sync settled), and focus the terminal that was
  // just opened. Runs on every open/close, tab switch, height change and
  // settled sync.
  const prevOpen = React.useRef(null);
  React.useEffect(() => {
    const entry = _UT_entryOf(state, activeSessionId);
    const rec = recs.current.get(activeSessionId);
    if (entry.open && rec?.mounted) {
      fit(activeSessionId);
      if (_UT_needsSpawn(entry) && rec.syncing === 0) spawn(activeSessionId);
      if (prevOpen.current !== activeSessionId) focus(activeSessionId);
    }
    prevOpen.current = entry.open ? activeSessionId : null;
  }, [state, activeSessionId, height, syncTick, fit, spawn, focus]);

  // Tab list changes: drop the panel state and the instance of closed tabs
  // (`closeSession` in live.js already had Rust end the tab and its shell),
  // and open the panel of a new tab when "open with new tab" is on. A tab
  // restored at launch counts as new; its shell starts once it is active.
  const seen = React.useRef(new Set());
  const idsKey = sessionIds.join("\0");
  React.useEffect(() => {
    const fresh = sessionIds.filter((sid) => !seen.current.has(sid));
    seen.current = new Set(sessionIds);
    setState((s) => {
      let next = _UT_prune(s, sessionIds);
      if (tweaks.terminalAutoOpen) for (const sid of fresh) next = _UT_open(next, sid);
      return next;
    });
    for (const sid of [...recs.current.keys()]) if (!sessionIds.includes(sid)) dispose(sid);
  }, [idsKey, dispose]); // eslint-disable-line react-hooks/exhaustive-deps

  // Theme and font follow the tweaks for every instance.
  React.useEffect(() => {
    for (const [sid, rec] of recs.current) {
      rec.term.options.theme = _UT_theme();
      rec.term.options.fontSize = fontSize;
      fit(sid);
    }
  }, [tweaks.theme, tweaks.accent, fontSize, fit]);

  // Tweaks → Terminal shell: Rust's saved setting (`terminal.json`). A
  // refused path keeps the field's text and shows Rust's reason.
  const [shellSettings, setShellSettings] = React.useState(null);
  const [shellError, setShellError] = React.useState(null);
  React.useEffect(() => {
    if (!bridge) return undefined;
    let live = true;
    bridge.terminalSettings().then((s) => { if (live && s) setShellSettings(s); });
    return () => { live = false; };
  }, [bridge]);
  const saveShell = React.useCallback(async (value) => {
    if (!bridge) return;
    const shell = value.trim();
    if (shellSettings && shell === shellSettings.shell) { setShellError(null); return; }
    const res = await bridge.setTerminalShell(shell);
    if (res.ok) { setShellSettings(res.settings); setShellError(null); }
    else setShellError(res.error);
  }, [bridge, shellSettings]);

  return {
    state, toggle, close, respawn, setHeight, mountFor, focus, copySelection,
    shellSettings, shellError, saveShell,
  };
}

Object.assign(window, { useTerminal });
