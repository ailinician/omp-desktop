/* ui/use-edge-drag.js — a panel edge dragged with the left button to resize
   it (terminal-panel.jsx's top edge, browser-dock.jsx's left edge).

   useEdgeDrag({ begin, measure, commit }) → { size, handlers }:
   `begin(e)` on the press returns `{ start, ... }` (the size shown now,
   plus whatever `measure` needs); `measure(e, drag)` maps a move to the
   new size; `commit(size)` runs once when the drag ends with a size other
   than `start`. `size` is the size to show while dragging (null when not),
   so a move re-renders the panel only, and the saved size is written per
   drag, not per pointer move. Spread `handlers` on the handle: pointerup,
   pointercancel and a lost capture all end the drag (the first one
   counts), and only the pointer that started it moves it. The callbacks
   are read per event, so inline arrows are fine. */

(function () {
  function useEdgeDrag({ begin, measure, commit }) {
    const drag = React.useRef(null);
    const [size, setSize] = React.useState(null);

    const onPointerDown = (e) => {
      if (e.button !== 0 || drag.current) return;
      const d = begin(e);
      drag.current = { ...d, pointerId: e.pointerId, size: d.start };
      e.currentTarget.setPointerCapture(e.pointerId);
      setSize(d.start);
      e.preventDefault();
    };
    const onPointerMove = (e) => {
      const d = drag.current;
      if (!d || e.pointerId !== d.pointerId) return;
      d.size = measure(e, d);
      setSize(d.size);
    };
    // One commit, batched with the reset of the local size, so the panel
    // does not flash back.
    const end = (e) => {
      const d = drag.current;
      if (!d || e.pointerId !== d.pointerId) return;
      drag.current = null;
      setSize(null);
      if (d.size !== d.start) commit(d.size);
    };

    return {
      size,
      handlers: { onPointerDown, onPointerMove, onPointerUp: end, onPointerCancel: end, onLostPointerCapture: end },
    };
  }

  window.useEdgeDrag = useEdgeDrag;
})();
