/* ui/collapse-toggle.jsx — the expand/collapse control of a collapsible chat
   block (#44): tool cards and thinking. Which blocks are collapsed is
   decided in app/chat-collapse.js; this only shows the state and asks for
   the other one. Its icon is an SVG, so a manual selection across the block
   copies no text from it; chat.css sizes and turns it. */

const { Icon: _CollapseIcon } = window;

function CollapseToggle({ collapsed, onToggle }) {
  const label = collapsed ? "Expand" : "Collapse";
  return (
    <button type="button" className="btn icon ghost collapse-toggle"
      aria-expanded={!collapsed} aria-label={label} title={label}
      onClick={onToggle}>
      <_CollapseIcon name="chev" />
    </button>
  );
}

Object.assign(window, { CollapseToggle });
