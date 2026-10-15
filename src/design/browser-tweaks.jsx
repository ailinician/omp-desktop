/* ═════════════════════════════════════════════════════════════════════
   design/browser-tweaks.jsx — Tweaks → Browser's Chromium row: the browser
   new panels launch (Rust's `browser.json`, read with `browserSettings`
   each time the panel opens). A button naming it opens a menu of
   `automatic` (hint: what auto-detection finds now), the browsers installed
   here (Rust's list), the stored custom path and "other…" (Rust's file
   dialog); a pick is saved with `browserSetPath`. Rust reads the setting at
   each launch, so it applies to panels started afterwards. Shares the
   terminal shell's hook and menu (terminal-tweaks.jsx). Renders only the
   row: app-live.jsx puts it in its Browser section.
   ═════════════════════════════════════════════════════════════════════ */

const { usePathSetting: _BTW_useSetting, PathPicker: _BTW_Picker } = window;

const _BTW_PATH = {
  key: "browser.path",
  read: (bridge) => bridge.browserSettings(),
  save: (bridge, value) => bridge.browserSetPath(value),
  pick: (bridge) => bridge.browserPickPath(),
};

/** Tooltip: the stored path, else what `automatic` resolves to now. */
function _BTW_title(settings) {
  if (!settings) return undefined;
  if (settings.path) return settings.path;
  return settings.automatic ? `automatic: ${settings.automatic}` : "automatic: no Chromium-based browser found";
}

function BrowserTweaks({ bridge }) {
  const { settings, error, save, pick } = _BTW_useSetting(bridge, _BTW_PATH);
  const { label, rows } = window.OMP_BROWSER.pathMenu(settings);
  return (
    <_BTW_Picker name="chromium" label={label} title={_BTW_title(settings)} rows={rows}
      disabled={!settings} error={error ?? settings?.error ?? null} save={save} pick={pick} />
  );
}

Object.assign(window, { BrowserTweaks });
