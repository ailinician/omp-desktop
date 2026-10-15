/* ═════════════════════════════════════════════════════════════════════
   design/terminal-tweaks.jsx — the Tweaks panel's Terminal section: the
   shell (Rust's `terminal.json`, read with `terminalSettings` each time the
   panel opens): a button naming it opens a menu of `default`, the shells
   installed here (Rust's list) and "other…" (Rust's file dialog); a pick is
   saved with `setTerminalShell`. Then "open with new tab" and the shared
   panel height (set by dragging a panel's top edge).

   The setting hook and the button + menu are shared with Tweaks → Browser
   (browser-tweaks.jsx, the Chromium path) as `usePathSetting` / `PathPicker`.
   The menu opens inline, below the button: the panel scrolls and clips, so
   a floating pop-up would be cut off at its edge.
   ═════════════════════════════════════════════════════════════════════ */

const { TweakSection: _TT_Section, TweakRow: _TT_Row, TweakToggle: _TT_Toggle, Icon: _TT_Icon } = window;

/**
 * The "other…" pick in flight per setting (`api.key`), kept outside the
 * component: closing Tweaks unmounts it, and on Linux (rfd's GTK3 dialog
 * ignores the parent window) the panel can be closed and reopened while
 * the dialog is up. A remount waits for this before it reads the setting,
 * so it shows the pick, and a second "other…" does nothing until the
 * dialog is gone.
 */
const _TT_pickInFlight = new Map();

/**
 * One Rust-owned path setting: `api` is a module constant `{ key, read,
 * save, pick }` whose functions take the bridge (`read` → settings or null,
 * `save(bridge, value)` / `pick` → `{ ok, settings }` | `{ ok: false, error }`,
 * `settings` null from `pick` = the dialog was cancelled). Returns the
 * settings, the reason Rust refused the last change, `save(value)` and
 * `pick()` (the file dialog).
 */
function usePathSetting(bridge, api) {
  const [settings, setSettings] = React.useState(null);
  const [error, setError] = React.useState(null);
  React.useEffect(() => {
    if (!bridge) return undefined;
    let live = true;
    Promise.resolve(_TT_pickInFlight.get(api.key))
      .then(() => api.read(bridge))
      .then((s) => { if (live && s) setSettings(s); });
    return () => { live = false; };
  }, [bridge, api]);
  const apply = React.useCallback((res) => {
    if (!res.ok) { setError(res.error); return; }
    setError(null);
    if (res.settings) setSettings(res.settings);
  }, []);
  // Sent even for the ticked row: Rust's reply is the stored setting, which
  // the menu may not show.
  const save = React.useCallback(async (value) => {
    apply(await api.save(bridge, value));
  }, [bridge, api, apply]);
  const pick = React.useCallback(async () => {
    if (_TT_pickInFlight.has(api.key)) return;
    const pending = api.pick(bridge);
    _TT_pickInFlight.set(api.key, pending);
    try { apply(await pending); } finally { _TT_pickInFlight.delete(api.key); }
  }, [bridge, api, apply]);
  return { settings, error, save, pick };
}

/**
 * A Tweaks row naming a path setting (`name`, its row label) and its inline
 * menu: `rows` (`{ name, hint, path, value, checked }`, exactly one checked)
 * then "other…". `label` is the button's text, `title` its tooltip;
 * `disabled` until the setting is read. A row saves its `value`.
 */
function PathPicker({ name, label, title, rows, disabled, error, save, pick }) {
  const [open, setOpen] = React.useState(false);
  const rootRef = React.useRef(null);
  const menuRef = React.useRef(null);
  const triggerRef = React.useRef(null);

  const close = React.useCallback((refocus = true) => {
    setOpen(false);
    if (refocus) triggerRef.current?.focus();
  }, []);
  // An outside click leaves focus where it lands; Escape returns it.
  window.usePopoverDismiss(open, rootRef, () => close(false), close);

  // Focus the checked row when the menu opens, and keep the menu inside the
  // Tweaks panel's scroll area (the browser row sits near its bottom).
  React.useEffect(() => {
    if (!open) return;
    menuRef.current?.scrollIntoView({ block: "nearest" });
    menuRef.current?.querySelector('[aria-checked="true"]')?.focus({ preventScroll: true });
  }, [open]);

  // Arrows, Home and End move between the rows; Tab closes the menu.
  const onMenuKey = e => window.menuKeyNav(e, menuRef.current, "button", close);

  return (
    <div className="twk-row" ref={rootRef}>
      <_TT_Row label={name} inline>
        <button ref={triggerRef} type="button" className="twk-btn secondary twk-pick"
          aria-haspopup="menu" aria-expanded={open} disabled={disabled} title={title}
          onClick={() => (open ? close() : setOpen(true))}>
          {label}
          <_TT_Icon name="chev" size={10} color="var(--fg-4)" />
        </button>
      </_TT_Row>
      {open && (
        <div ref={menuRef} className="twk-menu" role="menu" aria-label={name} onKeyDown={onMenuKey}>
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
      {error != null && <div className="twk-path-error">{error}</div>}
    </div>
  );
}

const _TT_SHELL = {
  key: "terminal.shell",
  read: (bridge) => bridge.terminalSettings(),
  save: (bridge, value) => bridge.setTerminalShell(value),
  pick: (bridge) => bridge.pickTerminalShell(),
};

/** The shell setting. New shells use a pick; running ones keep theirs. */
function _TT_ShellPicker({ bridge }) {
  const { settings, error, save, pick } = usePathSetting(bridge, _TT_SHELL);
  const { label, rows } = window.OMP_TERMINAL.shellMenu(settings);
  return (
    <PathPicker name="shell" label={label} rows={rows} disabled={!settings} error={error}
      title={settings ? (settings.shell || `default: ${settings.defaultShell}`) : undefined}
      save={save} pick={pick} />
  );
}

/** `height` is the shared panel height (useTerminal's `height`). */
function TerminalTweaks({ bridge, height, autoOpen, onAutoOpen }) {
  return (
    <_TT_Section label="Terminal">
      <_TT_ShellPicker bridge={bridge} />
      <_TT_Toggle label="open with new tab" value={autoOpen} onChange={onAutoOpen} />
      <_TT_Row label="height" value={`${height} px`} />
    </_TT_Section>
  );
}

Object.assign(window, { TerminalTweaks, usePathSetting, PathPicker });
