/* ═════════════════════════════════════════════════════════════════════
   design/settings/rows-layout.jsx — custom Settings rows of Layout and
   Rail (window.OMP_SETTINGS_ROWS):
     widths   the saved column widths (sidebar/rail/tree/split/browser,
              `default` while a column has none) and "Reset all widths"
              (usePaneWidths' `reset`, the browser panel's width included)
     widgets  one switch per enabled widget plugin (`widget:<id>` tweaks via
              ctx.widgetTweakKey), one per extension widget toggle
              (ctx.extToggles), then widgets.json's path and "Reload widgets"
   The row frame and controls are settings/controls.jsx's.
   ═════════════════════════════════════════════════════════════════════ */

// The column-width tweaks in display order; 0 = the column's default.
const _SRL_WIDTHS = [
  { name: "sidebar", key: "sidebarWidth" },
  { name: "rail", key: "railWidth" },
  { name: "tree", key: "treeWidth" },
  { name: "split", key: "splitWidth" },
  // 0 = half the stage (OMP_BROWSER.clampWidth).
  { name: "browser", key: "browserWidth", fallback: "½ stage" },
];

// `modified` / `onReset` come from the page (the schema row's width `keys`).
function _SRL_Widths({ row, ctx, query, modified, onReset }) {
  const { SettingsRow: Row, SettingsButton: Button } = window;
  const { t } = ctx;
  return (
    <Row stack label={row.label ?? "Column widths"}
      desc={row.desc ?? "Drag a column edge to resize it, double-click the edge to reset one."}
      query={query} modified={modified} onReset={onReset}
      aside={<Button onClick={ctx.panes.reset} disabled={!modified}>Reset all widths</Button>}>
      <div className="st-widths">
        {_SRL_WIDTHS.map(w => {
          const px = t[w.key] ?? 0;
          return (
            <div key={w.key} className="st-width">
              <small>{w.name}</small>
              <b>
                {px > 0 ? <>{Math.round(px)}<i> px</i></> : <span className="st-width-def">{w.fallback ?? "default"}</span>}
                {px > 0 && <span className="st-moddot" title="Changed from default" />}
              </b>
            </div>
          );
        })}
      </div>
    </Row>
  );
}

/** One on-by-default switch row for tweak `key`. */
function _SRL_Switch({ tweak, label, desc, ctx, query }) {
  const { SettingsRow: Row, SettingsToggle: Toggle } = window;
  const on = ctx.t[tweak] ?? true;
  return (
    <Row label={label} desc={desc} query={query} modified={!on} onReset={() => ctx.setTweak(tweak, true)}>
      <Toggle value={on} label={label} onChange={v => ctx.setTweak(tweak, v)} />
    </Row>
  );
}

function _SRL_Widgets({ ctx, query }) {
  const { SettingsRow: Row, SettingsButton: Button, Icon } = window;
  const { widgets } = ctx;
  return (
    <>
      {widgets.togglePlugins.map(p => (
        <_SRL_Switch key={p.id} tweak={ctx.widgetTweakKey(p.id)} label={p.title} ctx={ctx} query={query}
          desc={`Plugin \`${(p.command ?? []).join(" ")}\` · ${p.scope === "global" ? "one card for all tabs" : "per tab"}.`} />
      ))}
      {ctx.extToggles.map(r => (
        <_SRL_Switch key={r.tweak} tweak={r.tweak} label={r.label} ctx={ctx} query={query}
          desc="A widget an omp extension shows in this rail." />
      ))}
      <Row stack label="Plugin config" query={query}
        desc="Plugins and their triggers. A changed script asks for approval again before it runs."
        aside={(
          <Button onClick={widgets.reload}>
            <Icon name="refresh" size={11} />Reload widgets
          </Button>
        )}>
        <div className="st-path">
          <Icon name="file" size={11} />
          <span className="st-path-text">{widgets.configPath ?? "unavailable"}</span>
        </div>
      </Row>
    </>
  );
}

Object.assign(window.OMP_SETTINGS_ROWS = window.OMP_SETTINGS_ROWS || {}, {
  widths: _SRL_Widths,
  widgets: _SRL_Widgets,
});
