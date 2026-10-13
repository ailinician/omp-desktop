/* ═════════════════════════════════════════════════════════════════════
   design/terminal-tweaks.jsx — the Tweaks panel's Terminal section: the
   shell path (Rust's `terminal.json`, read with `terminalSettings` each
   time the panel opens and saved with `setTerminalShell`), "open with new
   tab" and the shared panel height (set by dragging a panel's top edge).
   ═════════════════════════════════════════════════════════════════════ */

const { TweakSection: _TT_Section, TweakRow: _TT_Row, TweakToggle: _TT_Toggle } = window;

/**
 * Rust's saved shell setting (`{ shell, defaultShell }`), the reason it
 * refused the last save, and `save(value)`. A refused path keeps the
 * field's text and shows Rust's reason.
 */
function _TT_useShellSetting(bridge) {
  const [settings, setSettings] = React.useState(null);
  const [error, setError] = React.useState(null);
  React.useEffect(() => {
    if (!bridge) return undefined;
    let live = true;
    bridge.terminalSettings().then((s) => { if (live && s) setSettings(s); });
    return () => { live = false; };
  }, [bridge]);
  const save = React.useCallback(async (value) => {
    if (!bridge) return;
    const shell = value.trim();
    if (settings && shell === settings.shell) { setError(null); return; }
    const res = await bridge.setTerminalShell(shell);
    if (res.ok) { setSettings(res.settings); setError(null); }
    else setError(res.error);
  }, [bridge, settings]);
  return { settings, error, save };
}

/** `height` is the shared panel height (useTerminal's `height`). */
function TerminalTweaks({ bridge, height, autoOpen, onAutoOpen }) {
  const { settings, error, save } = _TT_useShellSetting(bridge);
  return (
    <_TT_Section label="Terminal">
      {/* Saved on blur (and Enter), not per keystroke: a half-typed path is
          not a shell. Rust validates and stores it; the next shell started
          uses it (running ones keep theirs). */}
      <_TT_Row label="shell">
        <input className="twk-field" type="text" spellCheck={false}
          key={settings?.shell ?? ""}
          defaultValue={settings?.shell ?? ""}
          placeholder={settings?.defaultShell || "default"}
          title={settings?.defaultShell ? `default: ${settings.defaultShell}` : undefined}
          aria-invalid={error != null}
          onBlur={e => save(e.target.value)}
          onKeyDown={e => { if (e.key === "Enter") e.currentTarget.blur(); }} />
        {error != null && <div className="term-shell-error">{error}</div>}
      </_TT_Row>
      <_TT_Toggle label="open with new tab" value={autoOpen} onChange={onAutoOpen} />
      <_TT_Row label="height" value={`${height} px`} />
    </_TT_Section>
  );
}

Object.assign(window, { TerminalTweaks });
