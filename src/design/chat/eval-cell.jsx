/* chat/eval-cell.jsx — one kernel cell inside an `eval` tool card.
   Applies hljs syntax highlighting once the cell is complete. While the
   cell is running the code is a partial string; highlighting it
   mid-stream produces malformed spans, so we stay with plain monospace
   until status flips to complete or error. */

const { CopyHost: _EvalCopyHost } = window;

function EvalCell({ cell, collapsed }) {
  const done = cell.status === "complete" || cell.status === "error";
  const lang = cell.language === "py" ? "python" : "javascript";
  const shownCode = cell.code ? (collapsed ? cell.code.split("\n", 5).join("\n") : cell.code.trimEnd()) : "";
  const shownOut  = cell.output && !(collapsed && shownCode)
    ? (collapsed ? cell.output.split("\n", 5).join("\n") : cell.output.trimEnd())
    : "";

  const codeHtml = React.useMemo(() => {
    if (!done || !shownCode || !window.hljs) return null;
    try {
      return window.hljs.highlight(shownCode, { language: lang }).value;
    } catch (_) { return null; }
  }, [done, shownCode, lang]);

  return (
    <div className={`eval-cell eval-${cell.status}`}>
      <div className="eval-cell-head mono">
        <span className="eval-lang">{cell.language}</span>
        {cell.title && <span className="eval-title">{cell.title}</span>}
        {cell.durationMs != null
          ? <span className="eval-dur" style={{ marginLeft: "auto" }}>{cell.durationMs}ms</span>
          : cell.status === "running" && <span className="shimmer-text" style={{ marginLeft: "auto", fontSize: 10 }}>running…</span>
        }
      </div>
      {shownCode && (
        <_EvalCopyHost label="Copy code" text={done ? cell.code.trimEnd() : ""}>
          {codeHtml
            ? <pre className="eval-code selectable"><code className={`hljs language-${lang}`}
                dangerouslySetInnerHTML={{ __html: codeHtml }} /></pre>
            : <pre className="eval-code selectable mono">{shownCode}</pre>}
        </_EvalCopyHost>
      )}
      {shownOut && (
        <_EvalCopyHost label="Copy output" text={done ? cell.output.trimEnd() : ""}>
          <pre className={`eval-output selectable mono${cell.status === "error" ? " eval-error" : ""}`}>
            {shownOut}
          </pre>
        </_EvalCopyHost>
      )}
    </div>
  );
}

Object.assign(window, { EvalCell });
