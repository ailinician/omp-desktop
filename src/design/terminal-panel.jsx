/* ═════════════════════════════════════════════════════════════════════
   design/terminal-panel.jsx — one tab's terminal panel: a head (shell,
   folder, exit state, close), the node xterm mounts into, and a drag
   handle on the top edge. Hidden tabs and collapsed panels stay mounted
   (`hidden`), so the xterm keeps its output; app/use-terminal.jsx owns
   the xterm and hands over `mountRef`; `onContextMenu` is the body's
   right-click (copy the selection).
   ═════════════════════════════════════════════════════════════════════ */

const { Icon: _TP_Icon } = window;
const { clampHeight: _TP_clamp, isExited: _TP_isExited } = window.OMP_TERMINAL;

function TerminalPanel({ entry, height, hidden, onHeight, onClose, mountRef, onContextMenu, shellLabel, cwd }) {
  const dragStart = (e) => {
    const column = e.currentTarget.closest(".session");
    const columnHeight = column ? column.getBoundingClientRect().height : window.innerHeight;
    const startY = e.clientY;
    const startHeight = height;
    const move = (ev) => onHeight(_TP_clamp(startHeight + startY - ev.clientY, columnHeight));
    const stop = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", stop);
      window.removeEventListener("pointercancel", stop);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", stop);
    window.addEventListener("pointercancel", stop);
    e.preventDefault();
  };

  return (
    <section className="term-panel" hidden={hidden} style={{ height }}>
      <div className="term-handle" onPointerDown={dragStart} />
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
