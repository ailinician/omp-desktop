/* ═════════════════════════════════════════════════════════════════════
   design/path-setting.jsx — a Rust-owned program path setting in Tweaks
   (Terminal's shell, Browser's Chromium): `usePathSetting` reads and saves
   it, `PathPicker` is the row's button and its menu (rows from
   app/path-setting.js `menu`, then "other…", Rust's file dialog).
   The menu opens inline, below the button: the panel scrolls and clips, so
   a floating pop-up would be cut off at its edge.
   ═════════════════════════════════════════════════════════════════════ */

const { TweakRow: _PSet_Row, Icon: _PSet_Icon } = window;

/**
 * The "other…" pick in flight per setting (`api.key`), kept outside the
 * component: closing Tweaks unmounts it, and on Linux (rfd's GTK3 dialog
 * ignores the parent window) the panel can be closed and reopened while
 * the dialog is up. A remount waits for this before it reads the setting,
 * so it shows the pick, and a second "other…" does nothing until the
 * dialog is gone.
 */
const _PSet_pickInFlight = new Map();

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
    Promise.resolve(_PSet_pickInFlight.get(api.key))
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
    if (_PSet_pickInFlight.has(api.key)) return;
    const pending = api.pick(bridge);
    _PSet_pickInFlight.set(api.key, pending);
    try { apply(await pending); } finally { _PSet_pickInFlight.delete(api.key); }
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
      <_PSet_Row label={name} inline>
        <button ref={triggerRef} type="button" className="twk-btn secondary twk-pick"
          aria-haspopup="menu" aria-expanded={open} disabled={disabled} title={title}
          onClick={() => (open ? close() : setOpen(true))}>
          {label}
          <_PSet_Icon name="chev" size={10} color="var(--fg-4)" />
        </button>
      </_PSet_Row>
      {open && (
        <div ref={menuRef} className="twk-menu" role="menu" aria-label={name} onKeyDown={onMenuKey}>
          {rows.map(r => (
            <button key={r.value || "default"} type="button" className="twk-menu-item"
              role="menuitemradio" aria-checked={r.checked} title={r.path || undefined}
              onClick={() => { close(); save(r.value); }}>
              <span className="twk-menu-mark">
                {r.checked && <_PSet_Icon name="check" size={10} color="var(--accent)" />}
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

Object.assign(window, { usePathSetting, PathPicker });
