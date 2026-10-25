/* ═════════════════════════════════════════════════════════════════════
   chrome.jsx — window chrome, tabs, status bar, ambient peripherals
   - WindowChrome (traffic lights, title)
   - TabBar
   - StatusBar
   - Ambient rail: TokenGauge, ActivityRadar, subagents card, Minimap
   ═════════════════════════════════════════════════════════════════════ */

const { Icon, TokenGauge, ActivityRadar, Sparkline, TOOL_META, ProfileMenu, DEFAULT_PROFILE_ID, SubagentRailCard, TabGroupChip, TabRunDot, TabLabel, RenameField } = window;
const { groupTabs } = window.OMP_PROJECT_NAV;

// Thin wrappers around the shared `OMP_KEYMAP.hintFor`/`hintKeyFor` — same
// guard and rationale as composer.jsx's copy (kept local rather than a
// third file to import from; see composer.jsx's doc comment for why the
// registry-not-loaded fallback can't move into keymap.js itself).
function hintFor(actionId, fallback) {
  return window.OMP_KEYMAP ? window.OMP_KEYMAP.hintFor(actionId) : fallback;
}

// ── Platform detection ────────────────────────────────────────────────
const IS_WIN = typeof navigator !== "undefined" &&
  (navigator.userAgent.includes("Windows") || navigator.platform.startsWith("Win"));

// ── Window chrome ─────────────────────────────────────────────────────
function WindowChrome({
  project, onCmd,
  profiles, activeProfileId, startupProfileId,
  onSelectProfile, onCreateProfile, onRenameProfile, onDeleteProfile, onSetStartupProfile,
}) {
  return (
    <div className="chrome" data-tauri-drag-region>
      {/* macOS traffic lights — left side, hidden on Windows */}
      {!IS_WIN && (
        <div className="chrome-lights">
          <span className="light red" />
          <span className="light amber" />
          <span className="light green" />
        </div>
      )}

      <div className="chrome-title">
        <span className="mono" style={{ color: "var(--fg-3)" }}>OMP</span>
        <span className="mono" style={{ color: "var(--fg-5)" }}>·</span>
        <span style={{ color: "var(--fg-2)" }}>{project.name}</span>
        {project.branch && (
          <span className="chip muted" style={{ marginLeft: 6 }}>
            <Icon name="branch" size={9} color="var(--fg-3)" />
            <span className="mono" style={{ color: "var(--fg-3)" }}>{project.branch}</span>
          </span>
        )}
      </div>

      {/* Per-tab omp profile — the selector acts on the active tab only. */}
      <ProfileMenu
        profiles={profiles}
        activeId={activeProfileId}
        startupId={startupProfileId}
        onSelect={onSelectProfile}
        onCreate={onCreateProfile}
        onRename={onRenameProfile}
        onDelete={onDeleteProfile}
        onSetStartup={onSetStartupProfile}
      />

      <div className="chrome-right">
        <button className="btn ghost outlined" onClick={onCmd}>
          <Icon name="command" size={11} /> {_t("bridge")}{" "}
          <span className="kbd">{hintFor("desktop.commands.open", IS_WIN ? "^K" : "⌘K")}</span>
        </button>

        {/* Windows controls — right side, hidden on macOS/Linux */}
        {IS_WIN && (
          <div className="win-controls">
            <button className="win-ctrl win-min" title={_t("Minimize")}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1 5.5h8" stroke="currentColor" strokeWidth="1" /></svg>
            </button>
            <button className="win-ctrl win-max" title={_t("Maximize / Restore")}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><rect x="1.5" y="1.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" /></svg>
            </button>
            <button className="win-ctrl win-close" title={_t("Close")}>
              <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true"><path d="M1.5 1.5l7 7M8.5 1.5l-7 7" stroke="currentColor" strokeWidth="1.1" /></svg>
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

// ── Project tabs ─────────────────────────────────────────────────────
// A tab's click props: select on click, close on middle-click, as in
// browsers (#24). WebKit before Safari 18.2 / WebKitGTK 2.46 has no
// `auxclick` and reports it as a `click` with button 1, hence the check in
// onClick too; newer engines never send `click` for button 1, so this
// can't double-fire. Swallowing the middle-button mousedown stops Windows
// autoscroll and Linux primary-selection paste from firing on the tab.
function tabClickProps(onSelect, onClose) {
  return {
    onClick: e => (e.button === 1 ? onClose?.() : onSelect?.()),
    onMouseDown: e => { if (e.button === 1) e.preventDefault(); },
    onAuxClick: e => { if (e.button === 1) { e.preventDefault(); onClose?.(); } },
  };
}

function TabBar({
  projects, activeId, onSelect, onClose, onRename, onNew, onHistory, onNewInProject, onReorder,
  sidebarOpen, onToggleSidebar, railOpen, onToggleRail,
  profiles = [], appVersion, updateVersion, onUpdate, onCheckUpdate,
  settings, onSelectSettings, onCloseSettings,
}) {
  // Tabs can run under different profiles, so a tab whose profile is not the
  // built-in one is labelled with it — otherwise two tabs on the same folder
  // in different profiles look identical. Falls back to the raw id until the
  // profile list has loaded.
  const profileLabel = id =>
    id === DEFAULT_PROFILE_ID ? null : (profiles.find(p => p.id === id)?.name ?? id);
  // Tab whose name is being edited inline (#32); double-click enters it.
  const [renaming, setRenaming] = React.useState(null);
  // Drag to reorder (#40): projects along the bar ("projects", keyed by
  // group), and a group chip's members in its dropdown (see TabGroupChip).
  const reorder = window.useDragReorder(onReorder);
  return (
    <div className="tabs" onPointerDown={reorder.onPointerDown}>
      <button className={`tab-add ${sidebarOpen ? "on" : ""}`}
        title={`${_t("project sidebar")} (${hintFor("desktop.sidebar.toggle", "Ctrl+B")})`}
        aria-pressed={!!sidebarOpen}
        onClick={onToggleSidebar}>
        <Icon name="sidebar" size={11} />
      </button>
      {/* Tabs on one folder + profile share a chip (#27); a project with a
          single tab renders as a plain tab. */}
      {groupTabs(projects).map((group) => {
        if (group.tabs.length > 1) {
          return (
            <TabGroupChip key={group.key}
              group={group}
              activeId={activeId}
              profileLabel={profileLabel}
              onSelect={onSelect}
              onClose={onClose}
              onNewInProject={onNewInProject}
              onRename={onRename}
              reorder={reorder}
            />
          );
        }
        const p = group.tabs[0];
        const active = p.id === activeId;
        const profile = profileLabel(p.profile);
        return (
          <div key={p.id}
            {...reorder.itemProps("projects", group.key, "x")}
            className={`tab ${active ? "active" : ""}${reorder.dropClass("projects", group.key)}`}
            {...tabClickProps(() => onSelect(p.id), () => onClose?.(p.id))}
            // The first click of the pair already selected the tab.
            onDoubleClick={e => { if (onRename && !e.target.closest(".tab-close")) setRenaming(p.id); }}>
            <span className="tab-bar-mark" style={{ background: active ? p.color : "transparent" }} />
            <Icon name="folder" size={11} color={active ? p.color : "var(--fg-4)"} />
            <TabRunDot state={p.runState} />
            {renaming === p.id ? (
              <RenameField value={p.name}
                onCommit={name => onRename(p.id, name)}
                onClose={() => setRenaming(null)} />
            ) : (
              <TabLabel tab={p} className="tab-name" title={`${p.name}\n${_t("double-click to rename")}`} />
            )}
            {profile && (
              <span className="chip muted tab-profile" title={`${_t("profile")}: ${profile}`}>{profile}</span>
            )}
            <button className="tab-close" onClick={e => { e.stopPropagation(); onClose?.(p.id); }}><Icon name="close" size={9} /></button>
          </div>
        );
      })}
      {/* The Settings app tab (#37): pinned after the projects, never
          dragged; `changed` counts settings off their default. */}
      {settings?.open && (
        <div className={`tab app-tab ${settings.active ? "active" : ""}`}
          title={`${_t("Settings")} (${hintFor("desktop.settings.open", "Ctrl+,")})`}
          {...tabClickProps(onSelectSettings, onCloseSettings)}>
          <span className="tab-bar-mark" style={{ background: settings.active ? "var(--fg-3)" : "transparent" }} />
          <Icon name="cog" size={12} />
          <span className="tab-name">{_t("Settings")}</span>
          {settings.changed > 0 && (
            <span className="tab-count" title={`${settings.changed} ${_t("changed from default")}`}>{settings.changed}</span>
          )}
          <button className="tab-close" title={`${_t("close")} (${hintFor("desktop.tab.close", "Ctrl+W")})`}
            onClick={e => { e.stopPropagation(); onCloseSettings?.(); }}>
            <Icon name="close" size={9} />
          </button>
        </div>
      )}
      <button className="tab-add" title={_t("open project")} onClick={onNew}>
        <Icon name="plus" size={11} />
      </button>
      <button className="tab-add" title={`${_t("conversation history")} (${hintFor("desktop.history.open", "Ctrl+H")})`} onClick={onHistory}>
        <Icon name="clock" size={11} />
      </button>
      <div style={{ flex: 1 }} />
      <div className="tabs-right mono">
        {updateVersion && (
          <button className="btn outlined update-pill" onClick={onUpdate} title={`OMP Desktop v${updateVersion} ${_t("is available")}`}>
            <Icon name="arrowUp" size={9} />v{updateVersion}
          </button>
        )}
        <button className="btn ghost app-version" onClick={onCheckUpdate} title={_t("check for updates")}>
          {appVersion ? `v${appVersion}` : "—"}
        </button>
      </div>
      {onToggleRail && (
        <button className={`tab-add ${railOpen ? "on" : ""}`}
          title={_t("ambient rail")}
          aria-pressed={!!railOpen}
          onClick={onToggleRail}
          style={{ borderLeft: "1px solid var(--line-soft)", borderRight: 0 }}>
          <Icon name="sidebar" size={11} style={{ transform: "scaleX(-1)" }} />
        </button>
      )}
    </div>
  );
}

// ── Status bar (footer): connection, tokens, todos, panels ──────────
// The model and thinking level are not repeated here: the composer's
// pills below the prompt box show (and change) both. It spans the window
// under the workspace; the st-ctx meter, st-cost and st-tps cells (each
// with its separator) drop out when the window is narrow (layout/rail.css).
function StatusBar({ ctx, todoDone, todoTotal, onTodo, onSettings, settingsActive = false, onChanges, onRules, approvalMode, onStats, onTree, treeOpen = false, onTerminal, terminalOpen, onBrowser, browserOpen = false }) {
  const ttft = ctx.ttft;
  const tpsR = ctx.tpsReading;
  const ttftVal = ttft && ttft.value != null ? `${ttft.value.toFixed(1)}s` : "—";
  const ttftSuffix = ttft && ttft.kind === "average" ? _t("(avg)") : "";
  const tpsVal = tpsR && tpsR.value != null ? tpsR.value.toFixed(1) : String(ctx.tokensPerSec ?? 0);
  const tpsSuffix = tpsR && tpsR.kind === "average" ? _t("(avg)") : tpsR && tpsR.kind === "last" ? _t("(last)") : "";
  const ttftHot = Boolean(ttft && ttft.kind === "counting");
  const tpsHot = Boolean(tpsR && tpsR.kind === "live");
  return (
    <div className="status">
      <span className="status-cell"><span className="dot live" /> {_t("connected")}</span>
      <span className="status-sep">·</span>
      <span className="status-cell st-ctx mono">
        <span style={{ color: "var(--fg-3)" }}>{ctx.label}</span>
        <span className="status-bar-tube">
          <span className="status-bar-fill" style={{ width: `${ctx.pct}%` }} />
        </span>
        <span className="st-ctx-pct" style={{ color: "var(--fg-4)" }}>{(+ctx.pct).toFixed(1)}%</span>
      </span>
      <span className="status-sep st-cost">·</span>
      <span className="status-cell st-cost mono">
        <span style={{ color: "var(--fg-3)" }}>{_t("cost")}</span> {ctx.cost}
      </span>
      <span className="status-sep st-tps">·</span>
      <span className="status-cell st-tps mono" title={_t("TTFT (time to first token) · output throughput (tok/s)")}>
        <span style={{ color: "var(--fg-3)" }}>{_t("TTFT")}</span>
        <span style={{ color: ttftHot ? "var(--accent)" : "var(--fg-2)" }}>{ttftVal}{ttftSuffix && <span style={{ color: "var(--fg-4)" }}>{ttftSuffix}</span>}</span>
        <span className="status-sep">·</span>
        <span><span style={{ color: tpsHot ? "var(--accent)" : "var(--fg-2)" }}>{tpsVal}</span> t/s{tpsSuffix && <span style={{ color: "var(--fg-4)" }}>{tpsSuffix}</span>}</span>
      </span>
      <div className="status-gap" />
      <button className="status-cell btn ghost" onClick={onTodo} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="plan" size={11} color="var(--accent)" />
        <span style={{ color: "var(--accent)" }}>{_t("todo")} {todoDone}/{todoTotal}</span>
      </button>
      <span className="status-sep">·</span>
      <button className="status-cell btn ghost" onClick={onChanges} title={_t("git browser")} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="diff2" size={11} color="var(--fg-3)" />
      </button>
      <span className="status-sep">·</span>
      <button className={`status-cell btn ghost ${treeOpen ? "on" : ""}`} onClick={onTree} aria-pressed={treeOpen}
        title={_t("conversation tree (prompt-cache status)")} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="branch" size={11} color={treeOpen ? "var(--accent)" : "var(--fg-3)"} />
      </button>
      <span className="status-sep">·</span>
      <button className="status-cell btn ghost" onClick={onBrowser} aria-pressed={browserOpen}
        title={`${_t("browser panel")} (${hintFor("desktop.browser.toggle", "Ctrl+Shift+B")})`}
        style={{ height: 20, padding: "0 6px" }}>
        <Icon name="web" size={11} color={browserOpen ? "var(--accent)" : "var(--fg-3)"} />
      </button>
      <span className="status-sep">·</span>
      <ApprovalModeCell cell={approvalMode} onClick={onRules} />
      <span className="status-sep">·</span>
      <button className="status-cell btn ghost" onClick={onStats} title={_t("usage statistics")} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="cost" size={11} color="var(--fg-3)" />
      </button>
      <span className="status-sep">·</span>
      <button className={`status-cell btn ghost ${terminalOpen ? "on" : ""}`} onClick={onTerminal}
        title={`${_t("terminal")} (${hintFor("desktop.terminal.toggle", "Ctrl+`")})`}
        aria-pressed={!!terminalOpen} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="terminal" size={11} color={terminalOpen ? "var(--accent)" : "var(--fg-3)"} />
      </button>
      <span className="status-sep">·</span>
      <button className={`status-cell btn ghost ${settingsActive ? "on" : ""}`} onClick={onSettings}
        title={`${_t("Settings")} (${hintFor("desktop.settings.open", "Ctrl+,")})`}
        aria-pressed={settingsActive} style={{ height: 20, padding: "0 6px" }}>
        <Icon name="cog" size={11} color={settingsActive ? "var(--accent)" : "var(--fg-3)"} />
      </button>
    </div>
  );
}

// The approvals button: names the tab's running tool approval mode
// (`OMP_APPROVAL_MODE.statusCell`). Yolo as the target turns the glyph to
// `warn` and amber; a change waiting for the turn to end reads
// `running ⇢ target` muted, with the target coloured.
function ApprovalModeCell({ cell, onClick }) {
  const targetColor = cell?.warn ? "var(--amber)" : "var(--fg-3)";
  return (
    <button className="status-cell btn ghost" onClick={onClick}
      title={cell?.pending ? `${_t("approvals")} — ${_t("switches to")} ${cell.target}` : _t("approvals")}
      style={{ height: 20, padding: "0 6px" }}>
      <Icon name={cell?.warn ? "warn" : "check"} size={11} color={targetColor} />
      {cell && (cell.pending
        ? <span className="mono status-mode" style={{ color: "var(--fg-4)" }}>{cell.running} ⇢ <span style={{ color: targetColor }}>{cell.target}</span></span>
        : <span className="mono status-mode" style={{ color: targetColor }}>{cell.label}</span>)}
    </button>
  );
}

// ── Minimap of the session: dense grid, one cell per message ─────────
// Hue encodes role/tool color (same palette as the chat). For assistant
// messages, opacity is log-scaled by tokens used so expensive turns pop
// against cheap ones. Click scrolls the chat to that message; hover
// highlights it via shared hoveredIdx state.
const SessionMinimap = React.memo(function SessionMinimap({ messages, hoveredIdx, onHover, onClick }) {
  // Log-scaled max across assistant messages so heatmap variance is
  // visible even when one compaction turn dwarfs the rest.
  const { maxTokens, shown } = React.useMemo(() => {
    let max = 0;
    let n = 0;
    for (const m of messages) {
      if (window.OMP_TURN_STATUS.isHidden(m)) continue; // the chat hides it too
      n++;
      if (m.kind === "assistant" && m.tokens && m.tokens > max) max = m.tokens;
    }
    return { maxTokens: max, shown: n };
  }, [messages]);
  const logMax  = Math.log10(maxTokens + 1) || 1;
  const mmStart = Math.max(0, messages.length - 260);

  return (
    <div className="minimap">
      <div className="minimap-head">
        <Icon name="minimap" size={11} color="var(--fg-3)" />
        <span className="mono" style={{ color: "var(--fg-3)" }}>{_t("session")}</span>
        <span className="mono" style={{ marginLeft: "auto", color: "var(--fg-4)" }}>{shown}</span>
      </div>
      <div className="minimap-grid">
        {messages.slice(mmStart).map((m, off) => {
          const i = mmStart + off;
          if (window.OMP_TURN_STATUS.isHidden(m)) return null;
          let hue = "var(--fg-5)";
          if      (m.kind === "user")      hue = "var(--magenta)";
          else if (m.kind === "assistant") hue = m.failure ? "var(--rose)" : "var(--accent)";
          else if (m.kind === "ask")       hue = "var(--amber)";
          else if (m.kind === "retry")     hue = TOOL_META.retry.color;
          else if (m.kind === "tool")      hue = TOOL_META[m.tool]?.color || "var(--fg-4)";
          else if (m.kind === "job")       hue = TOOL_META.job.color;
          else if (m.kind === "goal")      hue = TOOL_META.goal.color;
          else if (m.kind === "approval")  hue = TOOL_META.approval.color;

          // Brightness: assistant cells modulate by log(tokens), others flat.
          let opacity = 0.7;
          if (m.kind === "assistant" && m.tokens && maxTokens > 0) {
            const t = Math.log10(m.tokens + 1) / logMax;
            opacity = 0.4 + 0.6 * Math.max(0, Math.min(1, t));
          }

          // Per-kind tooltip — tools don't carry their own tokens (the
          // LLM cost lives on the assistant message that invoked them),
          // so they get tool-specific info instead of a token chip.
          let title;
          if (m.kind === "assistant" && m.failure) {
            title = `${_t("request failed")}${m.failure.httpStatus != null ? ` · HTTP ${m.failure.httpStatus}` : ""}${m.time ? " · " + m.time : ""}`;
          } else if (m.kind === "assistant") {
            const tok  = m.tokens ? `${m.tokens.toLocaleString()} tok` : "—";
            const inOut = (m.tokensIn != null || m.tokensOut != null)
              ? ` (${(m.tokensIn ?? 0).toLocaleString()} in · ${(m.tokensOut ?? 0).toLocaleString()} out)`
              : "";
            title = `${_t("assistant")} · ${tok}${inOut}${m.time ? " · " + m.time : ""}`;
          } else if (m.kind === "tool") {
            const dur = m.duration ? ` · ${(m.duration / 1000).toFixed(1)}s` : "";
            const status = m.status === "running" ? ` · ${_t("running")}` : (m.status === "ok" ? "" : ` · ${m.status}`);
            title = `${m.tool ?? _t("tool")}${m.title ? " " + m.title : ""}${dur}${status}`;
          } else if (m.kind === "user") {
            const preview = m.text ? ` · ${m.text.slice(0, 80)}${m.text.length > 80 ? "…" : ""}` : "";
            title = `${_t("you")}${preview}`;
          } else if (m.kind === "job") {
            title = `${m.jobId} ${_t("finished")}${m.label ? " · " + m.label : ""}`;
          } else if (m.kind === "retry") {
            title = `${_t("retrying")} · ${_t("attempt")} ${m.attempt}${m.maxAttempts ? " of " + m.maxAttempts : ""}`;
          } else if (m.kind === "goal") {
            title = window.OMP_GOAL.rowView(m).title;
          } else if (m.kind === "approval") {
            title = `${_t("auto-approved")} · ${m.tool || _t("tool")}`;
          } else {
            title = m.kind;
          }
          const cls = `minimap-cell ${m.kind} ${m.streaming ? "live" : ""} ${hoveredIdx === i ? "hot" : ""}`.trim();

          return (
            <div key={m._id ?? i}
              className={cls}
              style={{ background: hue, opacity }}
              title={title}
              onMouseEnter={() => onHover?.(i)}
              onMouseLeave={() => onHover?.(null)}
              onClick={() => onClick?.(i)} />
          );
        })}
      </div>
      <div className="minimap-legend">
        <span className="minimap-legend-item"><i style={{ background: "var(--magenta)" }} />{_t("you")}</span>
        <span className="minimap-legend-item"><i style={{ background: "var(--accent)" }} />{_t("assistant")}</span>
        <span className="minimap-legend-item"><i style={{ background: "var(--amber)" }} />{_t("ask")}</span>
        <span className="minimap-legend-item"><i style={{ background: "var(--fg-4)" }} />{_t("tool")}</span>
      </div>
    </div>
  );
}, (prev, next) => {
  if (prev.hoveredIdx !== next.hoveredIdx || prev.onHover !== next.onHover || prev.onClick !== next.onClick) return false;
  const a = prev.messages, b = next.messages;
  if (a === b) return true;
  if (!a || !b || a.length !== b.length) return false;
  if (a.length === 0) return true;
  const la = a[a.length - 1], lb = b[b.length - 1];
  return la._id === lb._id && la.kind === lb.kind && la.streaming === lb.streaming && la.status === lb.status && la.tokens === lb.tokens && la.failure === lb.failure && a[0]._id === b[0]._id;
});

// ── Right rail: ambient peripherals stacked ──────────────────────────
// `show`: { context, radar, subagents, minimap } — false hides that card
// (Settings → Rail, keys showContext/showRadar/showSubagents/showMinimap);
// `children` (widget plugin cards) sit between subagents and the minimap.
function AmbientRail({
  ctx, activity, messages, microcopy, onClose, sparklineValues, hoveredMsgIdx, onMinimapHover, onMinimapClick,
  subagents, subagentPaneOpen, onOpenSubagent, onToggleSubagentPane, onOpenTree, onCompact, canCompact, compacting,
  show = {}, children,
}) {
  // Use live tps samples. Before the first turn, sparklineValues is all zeros
  // which renders as a flat baseline — honest, not fake random data.
  const sparkVals = (sparklineValues && sparklineValues.length > 0)
    ? sparklineValues
    : Array(30).fill(0);
  return (
    <aside className="rail">
      <div className="rail-head">
        <span className="mono" style={{ color: "var(--fg-3)" }}>{_t("ambient")}</span>
        <span className="rail-head-actions">
          <button className="btn icon ghost" onClick={onOpenTree} title={_t("conversation tree (prompt-cache status)")}>
            <Icon name="branch" size={10} />
          </button>
          <button className="btn icon ghost" onClick={onClose} title={_t("hide rail")}>
            <Icon name="close" size={10} />
          </button>
        </span>
      </div>

      {(show.context ?? true) && (
        <div className="rail-card glass">
          <div className="rail-card-head">
            <Icon name="context" size={11} color="var(--lilac)" />
            <span className="mono" style={{ color: "var(--fg-2)" }}>{_t("context window")}</span>
            {onCompact && (
              <button className="btn ghost" onClick={onCompact} disabled={!canCompact}
                style={{ marginLeft: "auto", height: 20, padding: "0 7px", fontSize: "var(--d-text-xs)", gap: 4, border: "1px solid var(--line)" }}
                title={_t("compact context window")}>
                <Icon name="context" size={10} color={compacting ? "var(--lilac)" : "var(--fg-3)"} />
                <span>{compacting ? _t("compacting…") : _t("compact context")}</span>
              </button>
            )}
          </div>
          <TokenGauge used={ctx.used} total={ctx.total} pct={ctx.pct}
            label={ctx.label} sub={`${_t("cost")} ${ctx.cost} · ${ctx.tokensPerSec} t/s`} />
          <div className="rail-spark">
            <Sparkline values={sparkVals} width={210} height={28} />
            <div className="rail-spark-foot mono">
              <span style={{ color: "var(--fg-4)" }}>{_t("throughput")}</span>
              <span style={{ color: "var(--accent)" }}>{ctx.tokensPerSec} t/s</span>
            </div>
          </div>
        </div>
      )}

      {(show.radar ?? true) && (
        <div className="rail-card glass">
          <div className="rail-card-head">
            <Icon name="radar" size={11} color="var(--accent)" />
            <span className="mono" style={{ color: "var(--fg-2)" }}>{_t("agent radar")}</span>
            <span className="chip muted mono" style={{ marginLeft: "auto" }}>{_t("last 60s")}</span>
          </div>
          <ActivityRadar activity={activity} tps={ctx.tokensPerSec} />
          <div className="legend">
            {Object.entries(TOOL_META).filter(([k]) => ["read","search","edit","bash"].includes(k)).map(([k, m]) => (
              <span key={k} className="legend-item">
                <span style={{ background: m.color }} /> {m.label}
              </span>
            ))}
          </div>
        </div>
      )}

      {(show.subagents ?? true) && (
        <SubagentRailCard agents={subagents} paneOpen={subagentPaneOpen}
          onOpen={onOpenSubagent} onTogglePane={onToggleSubagentPane} />
      )}

      {children}
      {(show.minimap ?? true) && (
        <div className="rail-card glass" style={{ flex: 1, display: "flex", flexDirection: "column", minHeight: 120 }}>
          <div className="rail-card-head">
            <Icon name="minimap" size={11} color="var(--fg-3)" />
            <span className="mono" style={{ color: "var(--fg-2)" }}>{_t("minimap")}</span>
          </div>
          <SessionMinimap messages={messages} hoveredIdx={hoveredMsgIdx} onHover={onMinimapHover} onClick={onMinimapClick} />
        </div>
      )}
    </aside>
  );
}

Object.assign(window, { WindowChrome, TabBar, StatusBar, AmbientRail, SessionMinimap });
