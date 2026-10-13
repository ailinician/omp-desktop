/* ═════════════════════════════════════════════════════════════════════
   design/terminal-panel.jsx — one tab's terminal panel: a head (shell,
   folder, exit state, close), the node xterm mounts into, and a drag
   handle on the top edge. Hidden tabs and collapsed panels stay mounted
   (`hidden`), so the xterm keeps its output; app/use-terminal.jsx owns
   the xterm and hands over `mountRef`; `onContextMenu` is the body's
   right-click (copy the selection). A drag resizes the panel from local
   state and reports the final height once (`onHeight`), so the saved
   tweak is written per drag, not per pointer move.
   ═════════════════════════════════════════════════════════════════════ */

const { Icon: _TP_Icon } = window;
const { clampHeight: _TP_clamp, isExited: _TP_isExited } = window.OMP_TERMINAL;

function TerminalPanel({ entry, height, hidden, onHeight, onClose, mountRef, onContextMenu, shellLabel, cwd }) {
  // `{ pointerId, startY, startHeight, columnHeight, height }` while
  // dragging; `dragHeight` is the height shown meanwhile.
  const drag = React.useRef(null);
  const [dragHeight, setDragHeight] = React.useState(null);

  const dragStart = (e) => {
    if (e.button !== 0 || drag.current) return;
    const column = e.currentTarget.closest(".session");
    drag.current = {
      pointerId: e.pointerId,
      startY: e.clientY,
      startHeight: height,
      height,
      columnHeight: column ? column.getBoundingClientRect().height : window.innerHeight,
    };
    e.currentTarget.setPointerCapture(e.pointerId);
    setDragHeight(height);
    e.preventDefault();
  };
  const dragMove = (e) => {
    const d = drag.current;
    if (!d || e.pointerId !== d.pointerId) return;
    d.height = _TP_clamp(d.startHeight + d.startY - e.clientY, d.columnHeight);
    setDragHeight(d.height);
  };
  // pointerup, pointercancel and a lost capture all end the drag; the first
  // one persists the last height shown (one setTweak, batched with the
  // reset of the local height, so the panel does not flash back).
  const dragEnd = (e) => {
    const d = drag.current;
    if (!d || e.pointerId !== d.pointerId) return;
    drag.current = null;
    setDragHeight(null);
    if (d.height !== d.startHeight) onHeight(d.height);
  };

  return (
    <section className="term-panel" hidden={hidden} style={{ height: dragHeight ?? height }}>
      <div className="term-handle" onPointerDown={dragStart} onPointerMove={dragMove}
        onPointerUp={dragEnd} onPointerCancel={dragEnd} onLostPointerCapture={dragEnd} />
      <div className="term-head">
        <_TP_Icon name="terminal" size={11} color="var(--fg-3)" />
        <span className="term-head-shell">{shellLabel}</span>
        {cwd && <span className="term-head-cwd" title={cwd}>{cwd}</span>}
        {_TP_isExited(entry) && (
          <span className="term-head-exited" title={entry.error ?? undefined}>
            {entry.error != null ? "failed to start"
              : `exited${entry.status.exitCode == null ? "" : ` (${entry.status.exitCode})`}`}
          </span>
        )}
        <button type="button" className="term-head-close" title="hide terminal" onClick={onClose}>
          <_TP_Icon name="close" size={9} />
        </button>
      </div>
      <div className="term-body" ref={mountRef} onContextMenu={onContextMenu} />
    </section>
  );
}

Object.assign(window, { TerminalPanel });
