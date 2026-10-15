/* ui/markdown.jsx — Markdown renderer using marked + highlight.js
   when available; falls back to plain text with HTML escaping. */

const { copyCodeBlockClick: _MdCopyClick } = window;

// Clicks in rendered markdown, delegated on its container: the code blocks'
// copy buttons, then links — `MarkdownLinkContext` carries a handler
// (app-live.jsx: the browser panel's localhost links), null for none.
const MarkdownLinkContext = React.createContext(null);
function useMarkdownClick() {
  const onLink = React.useContext(MarkdownLinkContext);
  return React.useCallback((e) => { _MdCopyClick(e); onLink?.(e); }, [onLink]);
}

const escapeText = (text) =>
  text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

// ── Markdown renderer — uses marked + highlight.js when available ─────────
const MarkdownContent = ({ text, streaming }) => {
  const html = React.useMemo(() => {
    if (!text) return '';
    if (!window.marked) {
      // marked not loaded — render plain text with escaped HTML
      return '<p>' + escapeText(text)
        .replace(/\n\n/g, '</p><p>').replace(/\n/g, '<br>') + '</p>';
    }
    try { return window.marked.parse(text); }
    catch (_) { return '<pre>' + escapeText(text) + '</pre>'; }
  }, [text]);

  const onClick = useMarkdownClick();
  return (
    <div
      className={`md-content selectable${streaming ? ' md-streaming' : ''}`}
      onClick={onClick}
      dangerouslySetInnerHTML={{ __html: html }}
    />
  );
};


Object.assign(window, { MarkdownContent, MarkdownLinkContext, useMarkdownClick });
