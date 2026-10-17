/* ═════════════════════════════════════════════════════════════════════
   design/settings/rows-approvals.jsx — custom Settings rows of Approvals
   (window.OMP_SETTINGS_ROWS):
     approvalMode   the tool approval mode editor (default + per-project
                    overrides, approval-mode-section.jsx) — the same
                    component the approvals panel shows
     approvalRules  how many standing rules the active tab's session and
                    project have (OMP_BRIDGE.listApprovalRules) and
                    "Manage rules", which opens the approvals panel
   ═════════════════════════════════════════════════════════════════════ */

/** The rendered tab's entry, null with no tab (EMPTY_PROJECT has no id). */
function _SRAp_project(ctx) {
  return ctx.activeProject?.id ? ctx.activeProject : null;
}

function _SRAp_Mode({ ctx, query }) {
  const { SettingsRow: Row, ApprovalModeSection } = window;
  return (
    <Row stack label="Tool approval mode" query={query}
      desc="When the agent asks before it runs a tool: a default for every project, and projects that override it. A change restarts omp in the affected open tabs; conversations continue.">
      <ApprovalModeSection view={ctx.approvalModes} activeProject={_SRAp_project(ctx)}
        sessions={ctx.sessions} query={query} />
    </Row>
  );
}

function _SRAp_Rules({ ctx, query }) {
  const { SettingsRow: Row, SettingsButton: Button, Icon } = window;
  const project = _SRAp_project(ctx);
  const [count, setCount] = React.useState(null);
  React.useEffect(() => {
    if (!ctx.bridge || !project) { setCount(null); return undefined; }
    let live = true;
    ctx.bridge.listApprovalRules().then(rules => { if (live) setCount(Array.isArray(rules) ? rules.length : null); });
    return () => { live = false; };
  }, [ctx.bridge, project?.id]);
  let lead = "Open a tab to see its session and project rules.";
  if (project && count !== null) {
    lead = `${count === 0 ? "No" : count} standing rule${count === 1 ? "" : "s"} for this tab's session and project.`;
  }
  return (
    <Row label="Always-allow rules" query={query} className="st-linkrow"
      desc={`${lead} Rules are only granted from a prompt; here you review and revoke them.`}>
      <Button accent onClick={ctx.openApprovals}>
        Manage rules <Icon name="external" size={11} />
      </Button>
    </Row>
  );
}

Object.assign(window.OMP_SETTINGS_ROWS = window.OMP_SETTINGS_ROWS || {}, {
  approvalMode: _SRAp_Mode,
  approvalRules: _SRAp_Rules,
});
