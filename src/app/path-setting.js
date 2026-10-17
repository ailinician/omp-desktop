// app/path-setting.js — the menu behind a Rust-owned program path setting
// (Settings → Terminal's shell, Settings → Browser's Chromium; the button and
// menu are design/path-setting.jsx). Pure: the feature modules map their
// Rust settings onto `menu` (OMP_TERMINAL.shellMenu, OMP_BROWSER.pathMenu).
//   programName(path, fallback)   `/bin/zsh` → `zsh`, `C:\…\msedge.exe` → `msedge`
//   menu({ stored, name, fallback, installed })   → { label, rows }
// Paths match through OMP_PROJECT_NAV.normPath: Windows drive/UNC paths
// ignoring case and `/` vs `\` (a setting typed as `C:\Windows\…` or
// `C:/Windows/…` names the listed `C:\WINDOWS\…`).
(function () {
  function programName(path, fallback) {
    if (typeof path !== "string") return fallback;
    return window.OMP_PROJECT_NAV.basename(path).replace(/\.exe$/i, "") || fallback;
  }

  /**
   * `stored` is the saved path ("" = the fallback); `name(path)` a row's
   * name for a path; `fallback` the `{ name, hint, path }` of the "" row
   * (`path`: what it runs now, "" when nothing); `installed` the
   * `{ name, path, hint? }` the menu offers. Returns `{ label, rows }`:
   * `label` names what runs (the stored program, else the fallback's);
   * `rows` are `{ name, hint, path, value, checked }` — the fallback row,
   * each installed program, and the stored path when it is none of them
   * (`hint` "other", so it stands apart from a listed program of the same
   * name), so the menu always shows what is in use; exactly one is checked.
   * `value` is what the setting stores; `path` the full program, for a
   * tooltip.
   */
  function menu({ stored, name, fallback, installed }) {
    const { normPath } = window.OMP_PROJECT_NAV;
    const rows = [{ name: fallback.name, hint: fallback.hint, path: fallback.path, value: "", checked: stored === "" }];
    for (const p of installed) {
      rows.push({ name: p.name, hint: p.hint ?? "", path: p.path, value: p.path, checked: stored !== "" && normPath(p.path) === normPath(stored) });
    }
    if (stored && !rows.some((r) => r.checked)) rows.push({ name: name(stored), hint: "other", path: stored, value: stored, checked: true });
    const shown = stored || fallback.path;
    return { label: shown ? name(shown) : fallback.name, rows };
  }

  window.OMP_PATH_SETTING = { programName, menu };
})();
