// Terminal panel: pure helpers behind the per-tab shell panel. Rust owns the
// tab's shell — when it spawns, in which folder, with which program, its
// output ring and its status (`terminal_*` commands, `terminal://data|exit/
// {id}` events). This file holds only view state: whether a tab's panel is
// shown, what Rust last said about its shell, and which keystrokes xterm
// must hand back to the global keymap.
//
// Panel state is `{ [sessionId]: { open, created, status, error } }`,
// immutable: every transition returns a new object, or the same one when
// nothing changes. `open` is only visibility — collapsing keeps the shell.
// `created` is set the first time the panel is opened and stays: from then
// on the tab owns an xterm instance, which the view keeps mounted (hidden)
// until the tab goes. `status` is Rust's `TerminalStatus` (`{ instance,
// shell, state: "starting"|"running"|"exited", exitCode }`) or null while
// the tab has no shell; `error` is why the last `terminal_open` failed.
// Neither an exit nor a failed start respawns by itself: the user presses
// Enter (`isExited`), and only a tab with no shell at all spawns on show
// (`needsSpawn`).
//
// Ordering: each spawn has a global, increasing `instance`. A status or
// exit of an older instance than the one shown is stale and ignored, and
// within one instance the state only moves forward (starting → running →
// exited), so a late `terminal_open` reply never hides an exit that
// already arrived.
//
// Exposes `window.OMP_TERMINAL`; dependency-free IIFE (AGENTS.md "IIFE
// rule"). design/terminal-panel.jsx and app/use-terminal.jsx destructure it
// at top level, so it must load before them.
(function () {
  const EMPTY = Object.freeze({});
  const CLOSED = Object.freeze({ open: false, created: false, status: null, error: null });

  const DEFAULT_HEIGHT = 240;
  const MIN_HEIGHT = 120;
  const MAX_HEIGHT_RATIO = 0.7;

  const STATE_RANK = { starting: 0, running: 1, exited: 2 };

  function entryOf(s, sid) {
    return (s && s[sid]) || CLOSED;
  }

  /** `{...s, [sid]: {...entry, ...patch}}`, or `s` itself when nothing changes. */
  function patch(s, sid, change) {
    const prev = entryOf(s, sid);
    const next = { ...prev, ...change };
    if (Object.keys(next).every((k) => next[k] === prev[k])) return s;
    return { ...s, [sid]: next };
  }

  const openPanel = (s, sid) => patch(s, sid, { open: true, created: true });
  const closePanel = (s, sid) => patch(s, sid, { open: false });
  const togglePanel = (s, sid) =>
    entryOf(s, sid).open ? closePanel(s, sid) : openPanel(s, sid);

  /**
   * Rust reported the tab's shell (`terminal_open`, `terminal_status`).
   * Ignored for a tab with no entry (closed meanwhile), for `null` (no
   * news), for an older instance than the one shown, and for an earlier
   * state of the same instance. Clears a previous start error.
   */
  function applyStatus(s, sid, status) {
    if (!s || !s[sid] || !status) return s;
    const cur = s[sid].status;
    if (cur) {
      if (status.instance < cur.instance) return s;
      if (status.instance === cur.instance
        && (STATE_RANK[status.state] ?? 0) < (STATE_RANK[cur.state] ?? 0)) return s;
      if (status.instance === cur.instance && status.state === cur.state
        && status.shell === cur.shell && status.exitCode === cur.exitCode) {
        return patch(s, sid, { error: null });
      }
    }
    return patch(s, sid, { status, error: null });
  }

  /**
   * Shell `instance` exited with `code` (null = killed by a signal). Only
   * the instance shown counts; any other is stale or not known yet (the
   * hook asks Rust for the status of a newer one).
   */
  function applyExit(s, sid, instance, code) {
    const cur = s && s[sid] && s[sid].status;
    if (!cur || cur.instance !== instance) return s;
    const exitCode = Number.isInteger(code) ? code : null;
    if (cur.state === "exited" && cur.exitCode === exitCode) return s;
    return patch(s, sid, { status: { ...cur, state: "exited", exitCode } });
  }

  /** `terminal_open` failed with `error`: shown like an exited shell. */
  function spawnFailed(s, sid, error) {
    if (!s || !s[sid]) return s;
    return patch(s, sid, { error: String(error || "unknown error") });
  }

  /** The panel is shown, its xterm exists, and the tab has never had a shell. */
  function needsSpawn(entry) {
    return Boolean(entry.open && entry.created && entry.status === null && entry.error === null);
  }

  /** The shell is gone (exited, or never started): Enter starts a new one. */
  function isExited(entry) {
    return entry.error !== null || entry.status?.state === "exited";
  }

  /** Drop entries of sessions that no longer exist. */
  function prune(s, sessionIds) {
    const live = new Set(sessionIds);
    const keys = Object.keys(s || {});
    if (keys.every((k) => live.has(k))) return s;
    const next = {};
    for (const k of keys) if (live.has(k)) next[k] = s[k];
    return next;
  }

  /**
   * True for the chords the global keymap must still handle while the
   * terminal has focus (xterm's `attachCustomKeyEventHandler` returns false
   * for them, so they bubble). Everything else — Escape, ctrl+c, ctrl+w (the
   * shell's kill-word), plain keys, super+c/v (copy/paste) — stays xterm's.
   * Normalises chords exactly like the global keymap (via OMP_KEYMAP.chordFromEvent)
   * so shifted variants of the same physical key (e.g. Cmd+Shift+7 → "/" on
   * German layouts) still bypass.
   * @param {Pick<KeyboardEvent, "key"|"ctrlKey"|"shiftKey"|"altKey"|"metaKey"|"isComposing">} e
   */
  const BYPASS = new Set([
    "super+t", "super+w", "super+k", "super+h", "super+b", "super+/", "super+`",
    "super+~", // shifted backtick variant
    "ctrl+tab", "ctrl+shift+tab",
    "ctrl+`",
  ]);

  function keymapShouldBypass(e) {
    const chord = window.OMP_KEYMAP.chordFromEvent(e);
    return chord !== null && BYPASS.has(chord);
  }

  /** `/bin/zsh` -> `zsh`; `C:\\Windows\\System32\\cmd.exe` -> `cmd`. */
  function shellLabel(path) {
    if (typeof path !== "string") return "shell";
    const base = path.replace(/[\\/]+$/, "").split(/[\\/]/).pop().replace(/\.exe$/i, "");
    return base || "shell";
  }

  /**
   * Panel height in px: whole pixels clamped to [0, 70% of column].
   * Non-finite px falls back to DEFAULT_HEIGHT before clamping.
   * When 0.7×column < 120 the column share wins (max-wins) so the panel
   * never exceeds its column, but the floor is always 0.
   */
  function clampHeight(px, columnHeight) {
    if (!Number.isFinite(px)) px = DEFAULT_HEIGHT;
    const max = Math.max(0, Math.floor(columnHeight * MAX_HEIGHT_RATIO) || 0);
    return Math.min(max, Math.max(MIN_HEIGHT, Math.round(px)));
  }

  window.OMP_TERMINAL = {
    EMPTY,
    DEFAULT_HEIGHT,
    openPanel,
    closePanel,
    togglePanel,
    applyStatus,
    applyExit,
    spawnFailed,
    needsSpawn,
    isExited,
    prune,
    entryOf,
    keymapShouldBypass,
    shellLabel,
    clampHeight,
  };
})();
