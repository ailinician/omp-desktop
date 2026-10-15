/* browser-dock.jsx — the browser panel's slot in the stage: the panel plus a
   drag handle on its left edge (docs/architecture/browser.md,
   "Panel"). Width = OMP_BROWSER.clampWidth(tweak, room), where room is the
   stage's width less the subagent split pane's column (`.split`, beside the
   chat in split view), so the chat keeps its minimum; the drag
   (ui/use-edge-drag.js) follows the pointer and is persisted once, on
   release. In full view the dock takes
   the whole stage (layout/browser.css hides the other columns).
   `stageRef` is the `.stage` element the dock sits in. */

const { BrowserPanel: _BD_Panel } = window;
const { clampWidth: _BD_clamp } = window.OMP_BROWSER;

function BrowserDock({ ui, stageRef }) {
  const [room, setRoom] = React.useState(0);

  // The stage resizes with the window; the split pane comes and goes as a
  // stage child without resizing the stage, so children are watched too.
  React.useLayoutEffect(() => {
    const stage = stageRef.current;
    if (!stage) return undefined;
    let split = null;
    const measure = () => {
      const splitW = split ? split.getBoundingClientRect().width : 0;
      setRoom(stage.getBoundingClientRect().width - splitW);
    };
    const ro = new ResizeObserver(measure);
    const track = () => {
      const next = stage.querySelector(":scope > .split");
      if (next !== split) {
        if (split) ro.unobserve(split);
        split = next;
        if (split) ro.observe(split);
      }
      measure();
    };
    const mo = new MutationObserver(track);
    ro.observe(stage);
    mo.observe(stage, { childList: true });
    track();
    return () => { ro.disconnect(); mo.disconnect(); };
  }, [stageRef]);

  const drag = window.useEdgeDrag({
    begin: () => ({ start: width }),
    measure: (e) => _BD_clamp(stageRef.current.getBoundingClientRect().right - e.clientX, room),
    commit: ui.setWidth,
  });
  const width = _BD_clamp(drag.size ?? ui.width, room);

  return (
    <div className="browser-dock" style={ui.fullView ? undefined : { width }}>
      {!ui.fullView && (
        <div
          className={`browser-resize${drag.size != null ? " dragging" : ""}`}
          role="separator"
          aria-orientation="vertical"
          title="drag to resize, double-click for the default width"
          {...drag.handlers}
          onDoubleClick={() => ui.setWidth(0)}
        />
      )}
      <_BD_Panel ui={ui} />
    </div>
  );
}

Object.assign(window, { BrowserDock });
