/* widgets/widget-blocks.jsx — renderers for the fixed widget block types
   (spec: docs/superpowers/specs/2026-10-12-rail-widgets-design.md).
   Everything is text; nothing from a plugin is interpreted as HTML. */

const { Icon: _WB_Icon } = window;
const { safeUrl: _wbSafeUrl, insertText: _wbInsertText, relativeTime: _wbRelativeTime } = window.OMP_WIDGETS;

function _wbOpen(url) {
  const safe = _wbSafeUrl(url);
  if (safe) window.OMP_BRIDGE?.openExternalUrl?.(safe);
}

function WidgetItem({ item, onInsert }) {
  const insert = _wbInsertText(item);
  const timeLabel = item.time != null ? _wbRelativeTime(item.time, Date.now()) : null;
  const timeIso = item.time != null ? new Date(item.time).toISOString() : null;
  const itemTitle = [item.label, item.detail].filter(Boolean).join("\n");
  return (
    <div className={`wdg-item tone-${item.tone}`}>
      <span className="wdg-dot" />
      <span className="wdg-item-text" title={itemTitle || undefined}>
        <span className="wdg-item-label selectable">{item.label}</span>
        {item.detail && <span className="wdg-item-detail selectable">{item.detail}</span>}
      </span>
      {timeLabel && <span className="wdg-item-time mono" title={timeIso || undefined}>{timeLabel}</span>}
      <span className="wdg-item-actions">
        {insert && (
          <button className="btn icon ghost" title="insert into prompt" onClick={() => onInsert?.(insert)}>
            <_WB_Icon name="plus" size={9} />
          </button>
        )}
        {item.url && (
          <button className="btn icon ghost" title={item.url} onClick={() => _wbOpen(item.url)}>
            <_WB_Icon name="external" size={9} />
          </button>
        )}
      </span>
    </div>
  );
}

function WidgetBlock({ block, onInsert }) {
  switch (block.type) {
    case "status":
      return (
        <div className={`wdg-status tone-${block.tone}`}>
          <span className="wdg-dot" />
          <span className="mono wdg-label">{block.label}</span>
          <span className="wdg-value selectable">{block.value}</span>
        </div>
      );
    case "number":
      return (
        <div className="wdg-number">
          <span className="wdg-number-value mono">{block.value}</span>
          <span className="wdg-label">{block.unit ? `${block.unit} · ` : ""}{block.label}</span>
        </div>
      );
    case "progress":
      return (
        <div className="wdg-progress">
          <div className="wdg-label mono">{block.label} <span className="wdg-value">{block.value}/{block.max}</span></div>
          <div className="wdg-tube"><div className="wdg-fill" style={{ width: `${(block.value / block.max) * 100}%` }} /></div>
        </div>
      );
    case "text":
      return (
        <div className="wdg-text-row">
          <div className="wdg-text selectable">{block.text}</div>
          {block.insert && (
            <button className="btn icon ghost" title="insert into prompt" onClick={() => onInsert?.(block.insert)}>
              <_WB_Icon name="plus" size={9} />
            </button>
          )}
        </div>
      );
    case "kv":
      return (
        <>
          {block.title && <div className="wdg-section mono">{block.title}</div>}
          <div className="wdg-kv">
            {block.rows.map((r, i) => (
              <React.Fragment key={i}>
                <span className="wdg-label mono">{r.k}</span>
                <span className="wdg-value selectable" title={r.v}>{r.v}</span>
              </React.Fragment>
            ))}
          </div>
        </>
      );
    case "list":
      return (
        <>
          {block.title && <div className="wdg-section mono">{block.title}</div>}
          <div className="wdg-list">{block.items.map((it, i) => <WidgetItem key={i} item={it} onInsert={onInsert} />)}</div>
        </>
      );
    case "link":
      return (
        <button className="btn ghost wdg-link" title={block.url} onClick={() => _wbOpen(block.url)}>
          <_WB_Icon name="external" size={9} /> {block.label}
        </button>
      );
    default:
      return null;
  }
}

function WidgetBlocks({ blocks, onInsert }) {
  return <>{blocks.map((b, i) => <WidgetBlock key={i} block={b} onInsert={onInsert} />)}</>;
}

Object.assign(window, { WidgetBlocks });
