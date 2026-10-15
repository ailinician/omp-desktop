/* browser-chips.jsx — the annotation chips above the composer's attachment
   strip (docs/architecture/browser.md, "Annotate"): one chip per
   saved browser annotation, `#1 button.save — "overflows on mobile"`, with ×
   to remove. Like the paste chips: the label is a button (when
   `onChipClick` is given; it scrolls the panel to the annotated element —
   the caller decides (app-live.jsx) whether the panel is on the page the
   chip was picked from) and the `#n selector` part is mono. A reserved
   chip is on its way out with a message that has not settled yet (a
   second submit would ship it twice): it is dimmed and marked as in
   flight. `dropped` is how many chip screenshots would not fit the image
   cap if the message went out now; their text blocks still go. */

const { Icon: _BC_Icon } = window;
const { chipLabel: _BC_label, MAX_IMAGES: _BC_CAP } = window.OMP_BROWSER;

/** `#n selector` in mono, then the comment part of `chipLabel` as text. */
function _BC_Text({ chip }) {
  const label = _BC_label(chip);
  const cut = label.indexOf(" — ");
  if (cut < 0) return <span className="browser-chip-text mono">{label}</span>;
  return (
    <span className="browser-chip-text">
      <span className="mono">{label.slice(0, cut)}</span>{label.slice(cut)}
    </span>
  );
}

function BrowserChips({ chips, dropped = 0, onRemove, onChipClick }) {
  if (!chips.length) return null;
  return (
    <div className="browser-chips">
      {chips.map((c) => {
        const title = c.reserved ? `${c.pick.url} — sending…` : c.pick.url;
        const body = (<><_BC_Icon name="web" size={10} /><_BC_Text chip={c} /></>);
        return (
          <span className={`browser-chip${c.reserved ? " reserved" : ""}`} key={c.id} title={title}>
            {onChipClick
              ? <button type="button" className="browser-chip-label" onClick={() => onChipClick(c)}>{body}</button>
              : <span className="browser-chip-label">{body}</span>}
            <button type="button" className="browser-chip-remove" title="remove annotation" aria-label="remove annotation"
              onClick={() => onRemove(c.id)}>
              <_BC_Icon name="close" size={9} />
            </button>
          </span>
        );
      })}
      {dropped > 0 && (
        <span className="browser-chips-note">
          {dropped} screenshot{dropped === 1 ? "" : "s"} over the {_BC_CAP}-image limit will not be attached
        </span>
      )}
    </div>
  );
}

Object.assign(window, { BrowserChips });
