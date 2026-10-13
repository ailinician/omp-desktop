/* ═════════════════════════════════════════════════════════════════════
   app/use-terminal.jsx — per-tab terminal lifecycle for app-live.jsx.
   One xterm `Terminal` per tab, kept in a Map ref and mounted into its
   panel's node (design/terminal-panel.jsx); a hidden tab's instance keeps
   receiving output, and an instance stays mounted until its tab goes. The
   shell is spawned the first time a tab's panel is open and mounted, so the
   pty starts at the fitted size. Panel state (open / created / spawned /
   exited) is the pure reducer in app/terminal.js.

   Returns `{ state, toggle, close, respawn, setHeight, mountFor, focus,
   copySelection }`. `tweaks.terminalShell` is handed to the bridge once at
   start; `tweaks.terminalHeight` is the panel height every panel shares.
   Every panel body is watched with a ResizeObserver: sidebar, tree,
   subagent pane and window changes all end in a (debounced) refit.
   ═════════════════════════════════════════════════════════════════════ */

const {
  EMPTY: _UT_EMPTY, DEFAULT_HEIGHT: _UT_DEFAULT_HEIGHT, openPanel: _UT_open, closePanel: _UT_close, togglePanel: _UT_toggle,
  markSpawned: _UT_markSpawned, markExited: _UT_markExited, prune: _UT_prune, entryOf: _UT_entryOf,
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

function useTerminal({ bridge, activeSessionId, sessionIds, tweaks, setTweak }) {
  const [state, setState] = React.useState(_UT_EMPTY);
  const stateRef = React.useRef(state);
  stateRef.current = state;

  // sid → { term, fit, mounted, spawning, gen, observer, fitTimer }; sid →
  // { data, exit } unlisten fns; sid → stable ref callback for the panel's
  // mount node. `gen` counts the shell spawns of the tab (see terminal.js).
  const recs = React.useRef(new Map());
  const unlistens = React.useRef(new Map());
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

  const spawn = React.useCallback(async (sid) => {
    const rec = recs.current.get(sid);
    if (!rec || rec.spawning || !bridge) return;
    rec.spawning = true;
    rec.gen += 1;
    const gen = rec.gen;
    const res = await bridge.terminalOpen(sid, rec.term.cols, rec.term.rows);
    rec.spawning = false;
    if (!recs.current.has(sid)) return; // tab closed meanwhile
    if (res?.ok) {
      setState((s) => _UT_markSpawned(s, sid, gen));
    } else {
      // Shown as an exited shell: Enter tries again.
      rec.term.write(`\r\n\x1b[31mcould not start the shell: ${res?.error ?? "unknown error"}\x1b[0m \u2014 press Enter to retry\r\n`);
      setState((s) => _UT_markExited(s, sid, null, gen));
    }
  }, [bridge]);

  const respawn = React.useCallback((sid) => {
    const rec = recs.current.get(sid);
    if (!rec) return;
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
      if (e.key === "Enter" && _UT_entryOf(stateRef.current, sid).exited) {
        respawn(sid);
        return false;
      }
      return !_UT_bypass(e);
    });
    term.onData((data) => bridge.terminalWrite(sid, data));
    term.onResize(({ cols, rows }) => bridge.terminalResize(sid, cols, rows));
    const un = {
      data: bridge.onTerminalData(sid, (bytes) => term.write(bytes)),
      exit: bridge.onTerminalExit(sid, (code) => {
        // The generation of the shell that exited: the one running now (or
        // still being started — its exit can beat `terminal_open`'s reply).
        const gen = recs.current.get(sid)?.gen ?? 0;
        setState((s) => _UT_markExited(s, sid, code, gen));
        const label = code == null ? "" : ` (code ${code})`;
        term.write(`\r\n\x1b[2mshell exited${label} \u2014 press Enter to restart\x1b[0m\r\n`);
      }),
    };
    unlistens.current.set(sid, un);
    const rec = { term, fit: fitAddon, mounted: false, spawning: false, gen: 0, observer: null, fitTimer: 0 };
    recs.current.set(sid, rec);
    return rec;
  }, [bridge, respawn]);

  const dispose = React.useCallback((sid) => {
    const un = unlistens.current.get(sid);
    if (un) { un.data(); un.exit(); unlistens.current.delete(sid); }
    const rec = recs.current.get(sid);
    if (rec) {
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

  // A visible panel: size the xterm to it, spawn the shell the first time,
  // and focus the terminal that was just opened. Runs on every open/close,
  // tab switch and height change.
  const prevOpen = React.useRef(null);
  React.useEffect(() => {
    const open = _UT_entryOf(state, activeSessionId).open;
    const rec = recs.current.get(activeSessionId);
    if (open && rec?.mounted) {
      fit(activeSessionId);
      const entry = _UT_entryOf(state, activeSessionId);
      if (!entry.spawned && !entry.exited) spawn(activeSessionId);
      if (prevOpen.current !== activeSessionId) focus(activeSessionId);
    }
    prevOpen.current = open ? activeSessionId : null;
  }, [state, activeSessionId, height, fit, spawn, focus]);

  // Tab list changes: drop the panel state and the instance of closed tabs
  // (`closeSession` in live.js already told the backend to kill the shell),
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

  // The saved shell goes to the bridge once at start (later edits come from
  // the Tweaks field, which calls `bridge.setTerminalShell` itself).
  React.useEffect(() => {
    bridge?.setTerminalShell(tweaks.terminalShell ?? "");
  }, [bridge]); // eslint-disable-line react-hooks/exhaustive-deps

  return { state, toggle, close, respawn, setHeight, mountFor, focus, copySelection };
}

Object.assign(window, { useTerminal });
