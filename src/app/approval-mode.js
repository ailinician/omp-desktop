/* ═════════════════════════════════════════════════════════════════════
   approval-mode.js — omp's tool approval mode (`--approval-mode`) as the
   desktop chooses it: a global default plus a per-project override, both
   stored in Rust (`approval_mode.rs`, `approval_mode_get` returns the
   `{ default, projects }` view). Pure decisions only:
     - which mode a tab's folder resolves to (`effectiveFor`),
     - what the status-bar cell shows (`statusCell`),
     - which open tabs a change restarts now / once idle (`restartPlan`),
     - the panel's "this project" segment (`projectRow`).
   live.js runs the restarts (`_applyApprovalModes`, `_respawnSession`);
   the approvals panel and the status bar render from these.
   Details: docs/architecture/approval-mode.md.
   ═════════════════════════════════════════════════════════════════════ */
(function () {
  const MODES = Object.freeze([
    Object.freeze({ value: "always-ask", label: "always ask", warn: false,
      blurb: "the agent asks before every file change and every command. Only reads run on their own. \"Allow for this session\" / \"Always allow in this project\" on a prompt still work." }),
    Object.freeze({ value: "write",      label: "write",      warn: false,
      blurb: "the agent edits files on its own and asks before shell commands and anything else that runs code. Reads never ask." }),
    Object.freeze({ value: "yolo",       label: "yolo",       warn: true,
      blurb: "every tool runs without asking, shell commands included. omp's own stop for dangerous commands (rm -rf /, fork bombs, …) is skipped too. Your standing rules are kept but never needed here." }),
  ]);
  const DEFAULT_MODE = "write";

  /** Exactly one of omp's kebab-case mode strings (`"Write"` is not one). */
  function isMode(s) {
    return MODES.some(m => m.value === s);
  }

  /** Display label of a mode; an unknown string is shown as itself. */
  function labelOf(mode) {
    return MODES.find(m => m.value === mode)?.label ?? mode;
  }

  /** The mode a tab in folder `path` spawns with: the project's override,
   *  else the global default. No view yet (before the boot fetch) reads as
   *  the desktop's default, which is also what Rust spawns with then. */
  function effectiveFor(view, path) {
    if (!view) return DEFAULT_MODE;
    const own = path ? view.projects?.[path] : undefined;
    if (isMode(own)) return own;
    return isMode(view.default) ? view.default : DEFAULT_MODE;
  }

  /** Status-bar cell for a tab: `running` is the label of the mode its omp
   *  runs (`approvalMode`, null until the first spawn resolves — then the
   *  target stands in), `target` the label of what its folder resolves to;
   *  `pending` while they differ (a change waiting for the tab to go idle).
   *  `label` is the one-line form (`running ⇢ target` when pending).
   *  `warn` follows the target, so yolo turns amber as soon as it is chosen. */
  function statusCell(view, entry) {
    const effective = effectiveFor(view, entry?.path || null);
    const running = isMode(entry?.approvalMode) ? entry.approvalMode : null;
    const pending = running !== null && running !== effective;
    const target = labelOf(effective);
    const shown = labelOf(running ?? effective);
    return {
      label: pending ? `${shown} ⇢ ${target}` : shown,
      running: shown,
      target,
      warn: effective === "yolo",
      pending,
    };
  }

  /** Tabs whose running mode differs from what their folder now resolves
   *  to: `now` for idle ones, `later` for busy ones (restarted when they
   *  next go idle). A failed tab has no process, and a tab with no
   *  running mode yet is still spawning — neither needs a restart. */
  function restartPlan(view, sessions) {
    const { isBusyRunState } = window.OMP_TURN_STATUS; // late-bound: turn-status.js loads after this file
    const now = [];
    const later = [];
    for (const s of Array.isArray(sessions) ? sessions : []) {
      if (!isMode(s?.approvalMode) || s.approvalMode === effectiveFor(view, s.path || null)) continue;
      if (s.runState === "failed") continue;
      (isBusyRunState(s.runState) ? later : now).push(s.id);
    }
    return { now, later };
  }

  /** The "this project" row: shown only for a tab with a folder; `value`
   *  is the override or the literal `"default"` (the segment that removes it). */
  function projectRow(view, path) {
    const own = path ? view?.projects?.[path] : undefined;
    return { present: !!path, value: isMode(own) ? own : "default" };
  }

  window.OMP_APPROVAL_MODE = Object.freeze({
    MODES, DEFAULT_MODE, isMode, labelOf, effectiveFor, statusCell, restartPlan, projectRow,
  });
})();
