/* ═════════════════════════════════════════════════════════════════════
   design/terminal-tweaks.jsx — the Tweaks panel's Terminal section: the
   shell (Rust's `terminal.json`, read with `terminalSettings` each time the
   panel opens): a button naming it opens a menu of `default`, the shells
   installed here (Rust's list) and "other…" (Rust's file dialog); a pick is
   saved with `setTerminalShell`. Then "open with new tab" and the shared
   panel height (set by dragging a panel's top edge). The setting hook and
   the button + menu are design/path-setting.jsx's.
   ═════════════════════════════════════════════════════════════════════ */

const { TweakSection: _TT_Section, TweakRow: _TT_Row, TweakToggle: _TT_Toggle, usePathSetting: _TT_useSetting, PathPicker: _TT_Picker } = window;

const _TT_SHELL = {
  key: "terminal.shell",
  read: (bridge) => bridge.terminalSettings(),
  save: (bridge, value) => bridge.setTerminalShell(value),
  pick: (bridge) => bridge.pickTerminalShell(),
};

/** The shell setting. New shells use a pick; running ones keep theirs. */
function _TT_ShellPicker({ bridge }) {
  const { settings, error, save, pick } = _TT_useSetting(bridge, _TT_SHELL);
  const { label, rows } = window.OMP_TERMINAL.shellMenu(settings);
  return (
    <_TT_Picker name={_t("shell")} label={label} rows={rows} disabled={!settings} error={error}
      title={settings ? (settings.shell || `default: ${settings.defaultShell}`) : undefined}
      save={save} pick={pick} />
  );
}

/** `height` is the shared panel height (useTerminal's `height`). */
function TerminalTweaks({ bridge, height, autoOpen, onAutoOpen }) {
  return (
    <_TT_Section label={_t("Terminal")}>
      <_TT_ShellPicker bridge={bridge} />
      <_TT_Toggle label={_t("open with new tab")} value={autoOpen} onChange={onAutoOpen} />
      <_TT_Row label={_t("height")} value={`${height} px`} />
    </_TT_Section>
  );
}

Object.assign(window, { TerminalTweaks });
