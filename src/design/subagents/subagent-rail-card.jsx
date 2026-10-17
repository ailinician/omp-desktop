/* subagents/subagent-rail-card.jsx — ambient summary of this session's
   subagents for the right rail (AmbientRail, between radar and minimap).
   Live agents first, then the most recent finished ones; a click opens the
   manager pane on that agent. */

const { Icon: _SAR_Icon, OMP_SUBAGENTS: _SAR, SaDot: _SAR_Dot, SaToolChip: _SAR_ToolChip,
  SaElapsed: _SAR_Elapsed, saHueStyle: _SAR_hue, useSaNow: _SAR_useNow } = window;

const SA_RAIL_MAX_ROWS = 5;

function SubagentRailCard({ agents, paneOpen, onOpen, onTogglePane }) {
  const _col = React.useState(false), collapsed = _col[0], setCollapsed = _col[1];
  const t = _SAR.totals(agents);
  const now = _SAR_useNow(t.live > 0, 500);
  // Live first (spawn order), then finished most-recent-first.
  const live = agents.filter(a => _SAR.isLive(a.status));
  const done = agents.filter(a => !_SAR.isLive(a.status)).sort((x, y) => (y.endedAt ?? 0) - (x.endedAt ?? 0));
  const rows = [...live, ...done].slice(0, SA_RAIL_MAX_ROWS);
  const hidden = agents.length - rows.length;

  return (
    <div className="rail-card glass">
      <div className="rail-card-head" style={{ cursor: "pointer", userSelect: "none" }} onClick={() => setCollapsed(c => !c)} title={collapsed ? _t("Expand subagents") : _t("Collapse subagents")}>
        <_SAR_Icon name="agent" size={11} color="var(--cyan)" />
        <span className="mono" style={{ color: "var(--fg-2)" }}>{_t("subagents")}</span>
        {t.live > 0 ? (
          <span className="chip accent mono" style={{ marginLeft: "auto" }}><span className="dot live" /> {t.live} {_t("live")}</span>
        ) : (
          <span className="chip muted mono" style={{ marginLeft: "auto" }}>{t.total ? `${t.total} ${_t("ran")}` : _t("idle")}</span>
        )}
        <_SAR_Icon name={collapsed ? "chevR" : "chev"} size={10} color="var(--fg-4)" />
      </div>

      {!collapsed && (
        <>
          {t.total === 0 ? (
            <div className="sa-empty">{_t("Agents spawned by ")}<span className="mono">task</span>{_t(" calls show up here, live.")}</div>
          ) : (
            <>
              <div className="sa-rail-stats mono">
                <span><b>{window.formatTokens(t.tokens)}</b> tok</span>
                <span><b>{_SAR.fmtCost(t.cost)}</b></span>
                <span><b>{t.tools}</b> {_t("tools")}</span>
                {t.failed > 0 && <span style={{ color: "var(--rose)" }}>{t.failed} {_t("failed")}</span>}
              </div>
              <div className="sa-rail-list">
                {rows.map(a => (
                  <button key={a.id} className="sa-rail-row" style={_SAR_hue(a)} onClick={() => onOpen(a.id)}
                    title={a.description ?? a.task ?? a.id}>
                    <_SAR_Dot status={a.status} />
                    <span className="sa-name">{a.id}</span>
                    <span className="sa-rail-now">
                      {a.status === "running" && a.progress?.currentTool ? (
                        <>
                          <_SAR_ToolChip tool={a.progress.currentTool} running />
                          <span>{a.progress.lastIntent ?? a.progress.currentToolArgs}</span>
                        </>
                      ) : (
                        <span>{a.description ?? a.task ?? a.agent}</span>
                      )}
                    </span>
                    <_SAR_Elapsed agent={a} now={now} />
                  </button>
                ))}
              </div>
              {hidden > 0 && <div className="sa-rail-more mono">+{hidden} {_t("more")}</div>}
            </>
          )}
          <button className="btn ghost outlined sa-rail-open" onClick={onTogglePane}>
            <_SAR_Icon name="split" size={11} /> {paneOpen ? _t("hide manager") : _t("open manager")}
          </button>
        </>
      )}
    </div>
  );
}

Object.assign(window, { SubagentRailCard });
