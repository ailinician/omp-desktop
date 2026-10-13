/* ═════════════════════════════════════════════════════════════════════
   design/terminal-tweaks.jsx — the Tweaks panel's Terminal section: the
   shell (Rust's `terminal.json`, read with `terminalSettings` each time the
   panel opens): a button naming it opens a menu of `default`, the shells
   installed here (Rust's list) and "other…" (Rust's file dialog); a pick is
   saved with `setTerminalShell`. Then "open with new tab" and the shared
   panel height (set by dragging a panel's top edge). The menu opens inline,
   below the button: the panel scrolls and clips, so a floating pop-up
   would be cut off at its edge.
   ═════════════════════════════════════════════════════════════════════ */

const { TweakSection: _TT_Section, TweakRow: _TT_Row, TweakToggle: _TT_Toggle, Icon: _TT_Icon } = window;

/**
 * The "other…" pick in flight, kept outside the section: closing Tweaks
 * unmounts it, and on Linux (rfd's GTK3 dialog ignores the parent window)
 * the panel can be closed and reopened while the dialog is up. A remount
 * waits for this before it reads the setting, so it shows the pick, and a
 * second "other…" does nothing until the dialog is gone.
 */
let _TT_pickInFlight = null;

/**
 * Rust's saved shell setting (`{ shell, defaultShell, shells }`), the reason
 * it refused the last change, `save(value)` and `pick()` (the file dialog).
 */
function _TT_useShellSetting(bridge) {
  const [settings, setSettings] = React.useState(null);
  const [error, setError] = React.useState(null);
  React.useEffect(() => {
    if (!bridge) return undefined;
    let live = true;
    Promise.resolve(_TT_pickInFlight)
      .then(() => bridge.terminalSettings())
      .then((s) => { if (live && s) setSettings(s); });
    return () => { live = false; };
  }, [bridge]);
  // `settings` null from `pick` = the dialog was cancelled.
  const apply = React.useCallback((res) => {
    if (!res.ok) { setError(res.error); return; }
    setError(null);
    if (res.settings) setSettings(res.settings);
  }, []);
  // Sent even for the ticked row: Rust's reply is the stored setting, which
  // the menu may not show.
  const save = React.useCallback(async (value) => {
    if (bridge) apply(await bridge.setTerminalShell(value));
  }, [bridge, apply]);
  const pick = React.useCallback(async () => {
    if (!bridge || _TT_pickInFlight) return;
    _TT_pickInFlight = bridge.pickTerminalShell();
    try { apply(await _TT_pickInFlight); } finally { _TT_pickInFlight = null; }
  }, [bridge, apply]);
  return { settings, error, save, pick };
}

/** The shell button and its inline menu. New shells use a pick; running ones keep theirs. */
function _TT_ShellPicker({ settings, error, save, pick }) {
  const [open, setOpen] = React.useState(false);
  const rootRef = React.useRef(null);
  const menuRef = React.useRef(null);
  const triggerRef = React.useRef(null);
  const { label, rows } = window.OMP_TERMINAL.shellMenu(settings);

  const close = React.useCallback((refocus = true) => {
    setOpen(false);
    if (refocus) triggerRef.current?.focus();
  }, []);
  // An outside click leaves focus where it lands; Escape returns it.
  window.usePopoverDismiss(open, rootRef, () => close(false), close);

  // Focus the checked row when the menu opens.
  React.useEffect(() => {
    if (open) menuRef.current?.querySelector('[aria-checked="true"]')?.focus();
  }, [open]);

  // Arrows, Home and End move between the rows; Tab closes the menu.
  const onMenuKey = e => {
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (e.key === "Tab") { e.preventDefault(); close(); return; }
    const all = [...menuRef.current.querySelectorAll("button")];
    const i = all.indexOf(document.activeElement);
    const next = e.key === "ArrowDown" ? all[(i + 1) % all.length]
      : e.key === "ArrowUp" ? all[i <= 0 ? all.length - 1 : i - 1]
      : e.key === "Home" ? all[0]
      : e.key === "End" ? all[all.length - 1]
      : null;
    if (!next) return;
    e.preventDefault();
    next.focus();
  };

  return (
    <div className="twk-row" ref={rootRef}>
      <div className="twk-row twk-row-h">
        <div className="twk-lbl"><span>shell</span></div>
        <button ref={triggerRef} type="button" className="twk-btn secondary twk-pick"
          aria-haspopup="menu" aria-expanded={open} disabled={!settings}
          title={settings ? (settings.shell || `default: ${settings.defaultShell}`) : undefined}
          onClick={() => (open ? close() : setOpen(true))}>
          {label}
          <_TT_Icon name="chev" size={10} color="var(--fg-4)" />
        </button>
      </div>
      {open && (
        <div ref={menuRef} className="twk-menu" role="menu" aria-label="shell" onKeyDown={onMenuKey}>
          {rows.map(r => (
            <button key={r.value || "default"} type="button" className="twk-menu-item"
              role="menuitemradio" aria-checked={r.checked} title={r.path || undefined}
              onClick={() => { close(); save(r.value); }}>
              <span className="twk-menu-mark">
                {r.checked && <_TT_Icon name="check" size={10} color="var(--accent)" />}
              </span>
              <span className="twk-menu-name">{r.name}</span>
              {r.hint && <span className="twk-menu-hint">{r.hint}</span>}
            </button>
          ))}
          <button type="button" className="twk-menu-item" role="menuitem"
            onClick={() => { close(); pick(); }}>
            <span className="twk-menu-mark" />
            <span className="twk-menu-name">other…</span>
          </button>
        </div>
      )}
      {error != null && <div className="term-shell-error">{error}</div>}
    </div>
  );
}

/** `height` is the shared panel height (useTerminal's `height`). */
function TerminalTweaks({ bridge, height, autoOpen, onAutoOpen }) {
  const shell = _TT_useShellSetting(bridge);
  return (
    <_TT_Section label="Terminal">
      <_TT_ShellPicker {...shell} />
      <_TT_Toggle label="open with new tab" value={autoOpen} onChange={onAutoOpen} />
      <_TT_Row label="height" value={`${height} px`} />
    </_TT_Section>
  );
}

Object.assign(window, { TerminalTweaks });
