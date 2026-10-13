/* ui/use-popover-dismiss.js — outside-click / Escape dismissal shared by the
   dropdown menus (profile-menu.jsx, tab-group-chip.jsx, thinking-menu.jsx,
   terminal-tweaks.jsx).

   usePopoverDismiss(open, rootRef, onOutside, onEscape = onOutside): while
   `open`, a mousedown outside `rootRef` calls `onOutside`, and Escape calls
   `onEscape`. Escape is prevented, so the window keymap (use-keymap.jsx
   skips defaultPrevented events) does not also abort the running turn. The
   listeners are attached only while open, and the callbacks are read
   through a ref, so an inline arrow does not re-attach them every render.

   menuKeyNav(e, menu, selector, close): a menu's onKeyDown. Arrows, Home
   and End move focus between the `selector` items in `menu`; Tab closes the
   menu, prevented so Shift+Tab does nothing else. Ctrl/Alt/Meta chords are
   left to the window keymap (Ctrl+Tab, …). */

(function () {
  function usePopoverDismiss(open, rootRef, onOutside, onEscape = onOutside) {
    const handlers = React.useRef(null);
    handlers.current = { onOutside, onEscape };

    React.useEffect(() => {
      if (!open) return undefined;
      const onDown = e => { if (!rootRef.current?.contains(e.target)) handlers.current.onOutside(); };
      const onKey  = e => { if (e.key === "Escape") { e.preventDefault(); handlers.current.onEscape(); } };
      document.addEventListener("mousedown", onDown);
      document.addEventListener("keydown", onKey);
      return () => {
        document.removeEventListener("mousedown", onDown);
        document.removeEventListener("keydown", onKey);
      };
    }, [open, rootRef]);
  }

  function menuKeyNav(e, menu, selector, close) {
    if (e.ctrlKey || e.altKey || e.metaKey) return;
    if (e.key === "Tab") { e.preventDefault(); close(); return; }
    const all = [...menu.querySelectorAll(selector)];
    if (!all.length) return;
    const i = all.indexOf(document.activeElement);
    const next = e.key === "ArrowDown" ? all[(i + 1) % all.length]
      : e.key === "ArrowUp" ? all[i <= 0 ? all.length - 1 : i - 1]
      : e.key === "Home" ? all[0]
      : e.key === "End" ? all[all.length - 1]
      : null;
    if (!next) return;
    e.preventDefault();
    next.focus();
  }

  window.usePopoverDismiss = usePopoverDismiss;
  window.menuKeyNav = menuKeyNav;
})();
