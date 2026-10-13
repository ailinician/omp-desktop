// Terminal panel: pure helpers behind the per-tab shell panel. Rust owns the
// pty (terminal_open/write/resize/close, `terminal://data|exit/{id}` events);
// this file decides the panel's state, which keystrokes xterm must hand back
// to the global keymap, and decodes the base64 pty frames.
//
// Panel state is `{ [sessionId]: { open, created, spawned, exited, exitCode,
// exitGen } }`, immutable: every transition returns a new object, or the same
// one when nothing changes. `open` is only visibility — collapsing keeps the
// shell (`spawned` stays true). `created` is set the first time the panel is
// opened and stays: from then on the tab owns an xterm instance, which the
// view keeps mounted (hidden) until the tab goes, so even a panel hidden
// while its first spawn was pending or failed comes back intact. `exited`
// stays set across close/reopen until `markSpawned` records the respawn, so
// reopening an exited panel shows the "shell exited" banner instead of
// silently starting a new shell.
//
// Spawn generations: every spawn attempt has a number (the hook counts
// them). `exitGen` is the generation an exit was seen in. A shell that dies
// at once can emit its exit before `terminal_open` resolves; `markSpawned`
// of the same generation must then keep that exit instead of clearing it,
// while a later generation (the respawn) does clear it.
//
// Exposes `window.OMP_TERMINAL`; dependency-free IIFE (AGENTS.md "IIFE
// rule"). design/terminal-panel.jsx and app/use-terminal.jsx destructure it
// at top level, so it must load before them.
(function () {
  const EMPTY = Object.freeze({});
  const CLOSED = Object.freeze({
    open: false, created: false, spawned: false, exited: false, exitCode: null, exitGen: null,
  });

  const DEFAULT_HEIGHT = 240;
  const MIN_HEIGHT = 120;
  const MAX_HEIGHT_RATIO = 0.7;

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
   * The shell of spawn generation `gen` is running. Any earlier exit is
   * history — unless it is this generation's own (the shell died before the
   * spawn call returned): then the exit stands.
   */
  function markSpawned(s, sid, gen) {
    const prev = entryOf(s, sid);
    if (prev.exited && gen != null && prev.exitGen === gen) {
      return patch(s, sid, { spawned: true });
    }
    return patch(s, sid, { spawned: true, exited: false, exitCode: null, exitGen: null });
  }

  /** The shell of generation `gen` exited. A tab with no entry (closed meanwhile) is ignored. */
  function markExited(s, sid, code, gen) {
    if (!s || !s[sid]) return s;
    return patch(s, sid, {
      exited: true,
      exitCode: Number.isInteger(code) ? code : null,
      exitGen: gen ?? null,
    });
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

  const B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const B64_INDEX = new Map([...B64].map((c, i) => [c, i]));

  /** Standard base64 (RFC 4648, padded or not) -> bytes. Throws on bad input. */
  function decodeB64(str) {
    if (typeof str !== "string") throw new TypeError("decodeB64: string expected");
    const body = str.replace(/=+$/, "");
    if (body.length % 4 === 1) throw new Error("decodeB64: invalid length");
    const out = new Uint8Array(Math.floor((body.length * 3) / 4));
    let acc = 0;
    let bits = 0;
    let at = 0;
    for (const c of body) {
      const v = B64_INDEX.get(c);
      if (v === undefined) throw new Error(`decodeB64: invalid character ${JSON.stringify(c)}`);
      acc = (acc << 6) | v;
      bits += 6;
      if (bits >= 8) {
        bits -= 8;
        out[at++] = (acc >> bits) & 0xff;
        acc &= (1 << bits) - 1;
      }
    }
    return out;
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
    markSpawned,
    markExited,
    prune,
    entryOf,
    keymapShouldBypass,
    decodeB64,
    shellLabel,
    clampHeight,
  };
})();
