/* widgets/widget-card.jsx — one rail card per plugin run, plus the card
   shown for a config-level error. State shape comes from
   app/use-widgets.jsx; blocks from widgets/widget-blocks.jsx. */

const { Icon: _WC_Icon, WidgetBlocks: _WC_Blocks } = window;

function _wcTime(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  return `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
}

function WidgetErrorCard({ title, error }) {
  return (
    <div className="rail-card glass wdg-card tone-error">
      <div className="rail-card-head">
        <span className="wdg-dot" />
        <span className="mono" style={{ color: "var(--fg-2)" }}>{title}</span>
      </div>
      <div className="wdg-error selectable">{error}</div>
    </div>
  );
}

function WidgetCard({ plugin, run, onRefresh, onInsert }) {
  const state = run?.state ?? "idle";
  const toneClass = state === "error" ? "tone-error" : state === "ok" ? `tone-${run.widget.status}` : "tone-neutral";
  const stale = state === "running" && run.widget;
  const meta = state === "running" ? "running…" : run?.durationMs != null ? `${run.durationMs} ms · ${_wcTime(run.at)}` : "";
  return (
    <div className={`rail-card glass wdg-card ${toneClass}${stale ? " wdg-stale" : ""}`}>
      <div className="rail-card-head">
        <span className={`wdg-dot${state === "running" ? " pulse-dot" : ""}`} />
        <span className="mono" style={{ color: "var(--fg-2)" }}>{plugin.title}</span>
        {meta ? <span className="chip muted mono wdg-meta">{meta}</span> : <span className="wdg-meta" />}
        <button className="btn icon ghost" title="refresh" disabled={state === "running"} onClick={onRefresh}>
          <_WC_Icon name="refresh" size={10} />
        </button>
      </div>
      {state === "error" && (
        <>
          <div className="wdg-error selectable">{run.error}</div>
          {run.lastOkAt && <div className="wdg-note mono">last ok {_wcTime(run.lastOkAt)}</div>}
        </>
      )}
      {(state === "ok" || stale) && run.widget && (
        <>
          <_WC_Blocks blocks={run.widget.blocks} onInsert={onInsert} />
          {run.widget.skipped > 0 && <div className="wdg-note mono">{run.widget.skipped} unsupported block{run.widget.skipped > 1 ? "s" : ""}</div>}
        </>
      )}
      {state === "idle" && <div className="wdg-note mono">not run yet</div>}
    </div>
  );
}

Object.assign(window, { WidgetCard, WidgetErrorCard });
