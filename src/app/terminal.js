// Terminal panel: pure helpers behind the per-tab shell panel. Rust owns the
// tab's shell — when it spawns, in which folder, with which program, its
// output ring and its status (`terminal_*` commands, `terminal://data|exit/
// {id}` events). This file holds only view state: whether a tab's panel is
// shown, what Rust last said about its shell, which keystrokes xterm must
// hand back to the global keymap and which are its copy / paste chords.
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
// Enter (`canRestart`), and only a tab with no shell at all spawns on show
// (`needsSpawn`).
//
// Ordering: each spawn has a global, increasing `instance`. A status or
// exit of an older instance than the one shown is stale and ignored, and
// within one instance the state only moves forward (starting → running →
// exited), so a late `terminal_open` reply never hides an exit that
// already arrived.
//
// Exposes `window.OMP_TERMINAL`; an IIFE (AGENTS.md "IIFE rule") that calls
// `window.OMP_KEYMAP.detectMac()` at load, so it loads after app/keymap.js
// (its other OMP_KEYMAP lookups, and `shellLabel`'s
// `OMP_PROJECT_NAV.basename`, happen at call time). design/terminal-panel.jsx
// and app/use-terminal.jsx destructure it at top level, so it must load
// before them.
(function () {
  const EMPTY = Object.freeze({});
  const CLOSED = Object.freeze({ open: false, created: false, status: null, error: null });

  // The default height is `TWEAK_DEFAULTS.terminalHeight` (app/constants.js).
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

  /**
   * The tab has a shell that is starting or running. `opened` is the
   * instance the hook's last successful `terminal_open` returned (0 if
   * none): newer than the status shown means a reply the panel state has
   * not rendered yet, and that shell counts as live already. Resizes are
   * sent only while this holds (no pty to resize otherwise).
   */
  function shellLive(entry, opened = 0) {
    if (opened > (entry.status?.instance ?? 0)) return true;
    return entry.error === null && entry.status !== null && entry.status.state !== "exited";
  }

  /**
   * Enter starts a new shell: the panel shows an exited shell (or a failed
   * start) and no newer one is live already (see `shellLive`), so a second
   * Enter while a respawn is in flight or not rendered yet does nothing.
   */
  function canRestart(entry, opened = 0) {
    return isExited(entry) && !shellLive(entry, opened);
  }

  // While xterm has focus, a keydown either goes to the shell or is handed
  // back to the page, where the global keymap (use-keymap.jsx) handles it.
  // The decision follows the *resolved* keymap, so rebinding an action in
  // Settings → Keyboard changes what the terminal lets through.
  const IS_MAC = window.OMP_KEYMAP.detectMac();

  /**
   * The terminal's copy / paste chord for this keydown: `"copy"`,
   * `"paste"`, or null. Linux and Windows use ctrl+shift+c / ctrl+shift+v
   * (plain ctrl+c / ctrl+v belong to the shell); macOS has ⌘C / ⌘V, which
   * xterm and the WebView already handle natively, so this returns null
   * there.
   * @param {Pick<KeyboardEvent, "key"|"code"|"ctrlKey"|"shiftKey"|"altKey"|"metaKey"|"isComposing">} e
   * @param {boolean} [isMac]
   */
  function clipboardAction(e, isMac = IS_MAC) {
    if (isMac) return null;
    const chord = window.OMP_KEYMAP.chordFromEvent(e);
    if (chord === "ctrl+shift+c") return "copy";
    if (chord === "ctrl+shift+v") return "paste";
    return null;
  }

  /** The platform's copy / paste chords: always the terminal's. */
  const RESERVED_MAC = new Set(["super+c", "super+v"]);
  const RESERVED_OTHER = new Set(["ctrl+shift+c", "ctrl+shift+v"]);

  /**
   * True when xterm must hand this keydown back to the global keymap
   * (`attachCustomKeyEventHandler` returns false for it, so it bubbles).
   * A chord goes to the app iff it resolves (`OMP_KEYMAP.lookup`) to a
   * global-scope action marked `fromTerminal` (switching tabs, leaving the
   * shell), or the chord has the `super` modifier, or — off macOS — both
   * `ctrl` and `shift`. Plain
   * `ctrl+<key>` and `alt+<key>` stay the shell's (ctrl+w kills a word,
   * ctrl+p is history), as do unbound chords and the platform's copy /
   * paste chords. Chords are normalised exactly like the global keymap
   * (`OMP_KEYMAP.chordFromEvent`), so shifted variants of the same
   * physical key (e.g. Cmd+Shift+7 → "/" on German layouts) match too.
   * @param {Pick<KeyboardEvent, "key"|"code"|"ctrlKey"|"shiftKey"|"altKey"|"metaKey"|"isComposing">} e
   * @param {boolean} [isMac]
   */
  function keymapShouldBypass(e, isMac = IS_MAC) {
    const keymap = window.OMP_KEYMAP;
    const chord = keymap.chordFromEvent(e);
    if (chord === null || (isMac ? RESERVED_MAC : RESERVED_OTHER).has(chord)) return false;
    const id = keymap.lookup(chord);
    if (id === null) return false;
    const action = keymap.KEYMAP_ACTIONS.find((a) => a.id === id);
    if (!action || action.scope !== "global") return false;
    if (action.fromTerminal) return true;
    const mods = chord.split("+").slice(0, -1);
    if (mods.includes("super")) return true;
    return !isMac && mods.includes("ctrl") && mods.includes("shift");
  }

  /** `/bin/zsh` -> `zsh`; `C:\\Windows\\System32\\cmd.exe` -> `cmd`. */
  function shellLabel(path) {
    if (typeof path !== "string") return "shell";
    return window.OMP_PROJECT_NAV.basename(path).replace(/\.exe$/i, "") || "shell";
  }

  /** A Windows drive path (`C:\…`, `C:/…`). */
  const DRIVE_PATH = /^[A-Za-z]:[\\/]/;
  const windowsKey = (p) => p.replace(/\//g, "\\").toLowerCase();

  /** Same program path: exact, or for two Windows drive paths ignoring case
   *  and `/` vs `\` (a 0.7.0 setting typed as `C:\Windows\…` or `C:/Windows/…`
   *  names the listed `C:\WINDOWS\…`). */
  function samePath(a, b) {
    return a === b || (DRIVE_PATH.test(a) && DRIVE_PATH.test(b) && windowsKey(a) === windowsKey(b));
  }

  /**
   * Tweaks → Terminal's shell button and menu for Rust's `TerminalSettings`
   * (`{ shell, defaultShell, shells }`): `{ label, rows }`. `label` names the
   * shell new terminals run (the stored one, else the default). `rows` are
   * `{ name, hint, path, value, checked }`: `default` (value "", `hint` the
   * default shell's name, so a row says what it runs), each installed shell,
   * and the stored shell when it is none of them (set before, or picked with
   * "other…"; `hint` "other", so it stands apart from a listed shell of the
   * same name), so the menu always shows what is in use. `path` is the full
   * program, for a tooltip. `value` is what `setTerminalShell` stores;
   * exactly one row is checked.
   */
  function shellMenu(settings) {
    const shell = settings?.shell ?? "";
    const defaultShell = settings?.defaultShell ?? "";
    const row = (name, path, value, hint = "") => ({ name, hint, path, value, checked: samePath(value, shell) });
    const rows = [row("default", defaultShell, "", defaultShell ? shellLabel(defaultShell) : "")];
    for (const s of settings?.shells ?? []) rows.push(row(s.name, s.path, s.path));
    if (shell && !rows.some((r) => r.checked)) rows.push(row(shellLabel(shell), shell, shell, "other"));
    return { label: shellLabel(shell || defaultShell), rows };
  }

  /**
   * A dragged panel height (finite px) for a session column `columnHeight`
   * px tall: rounded to whole pixels, at least MIN_HEIGHT (120) and at most
   * 70 % of the column (`floor`, never below 0). When 70 % of the column is
   * under 120 px the column share wins, so the panel never outgrows the
   * column's cap.
   */
  function clampHeight(px, columnHeight) {
    const max = Math.max(0, Math.floor(columnHeight * MAX_HEIGHT_RATIO) || 0);
    return Math.min(max, Math.max(MIN_HEIGHT, Math.round(px)));
  }

  window.OMP_TERMINAL = {
    EMPTY,
    openPanel,
    closePanel,
    togglePanel,
    applyStatus,
    applyExit,
    spawnFailed,
    needsSpawn,
    isExited,
    shellLive,
    canRestart,
    entryOf,
    keymapShouldBypass,
    clipboardAction,
    shellLabel,
    shellMenu,
    clampHeight,
  };
})();
