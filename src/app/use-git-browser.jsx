/* app/use-git-browser.jsx — data flow of the git browser (design/git/*.jsx):
   the Rust `git_browser_*` commands (via OMP_BRIDGE.gitBrowser*) → panes.
   Rust does every git read; this hook holds the view state (browsed
   commit, selection, expanded dirs, changed-only, filter) and fetches.

   Mounted only while the browser is open. Status and log load in parallel
   on mount; the file pane follows the selection (working-tree or commit
   variant by mode); a history click browses that commit (its touched files
   replace the tree, the first one is selected), the working-tree row
   returns. The working tree (status, its file) refetches on the refresh
   button, after stage/discard, when the active tab's branch changes and
   when an edit/write tool card finishes; the log on the refresh button and
   a branch change; a commit and its files never (a commit cannot change).
   A project change remounts the browser (app-live.jsx keys it). Each fetch
   is tagged with what it was for, so only the newest lands and a previous
   file never shows under a new selection. */

const {
  changedDirs: _UGB_changedDirs, ancestorDirs: _UGB_ancestorDirs, firstFile: _UGB_firstFile,
  discardDeletes: _UGB_discardDeletes,
} = window.OMP_GIT_BROWSER;

const _UGB_NO_PROJECT = "this tab has no project folder";

/** `{ value, error }` of a bridge result (`{ok, value | error}`, or null
 *  for a tab without a project folder). */
function _ugbResult(res) {
  if (!res) return { value: null, error: _UGB_NO_PROJECT };
  return res.ok ? { value: res.value, error: null } : { value: null, error: res.error };
}

/** `edits`: the count of finished edit/write cards (OMP_GIT_BROWSER
 *  `finishedEdits`); a rise means a file on disk changed. */
function useGitBrowser({ bridge, initialPath, branch, edits }) {
  const [nonce, setNonce] = React.useState(0);
  // A change refetches the log; with `edits`, the working tree too.
  const logTick = `${branch ?? ""}\0${nonce}`;
  const tick = `${logTick}\0${edits}`;
  const refresh = React.useCallback(() => setNonce(n => n + 1), []);

  // The browsed commit; null = the working tree.
  const [commitSha, setCommitSha] = React.useState(null);
  const [selected, setSelected] = React.useState(initialPath || null);
  const [expanded, setExpanded] = React.useState(() => new Set());
  const [changedOnly, setChangedOnly] = React.useState(false);
  const [filter, setFilter] = React.useState("");

  const [status, setStatus] = React.useState({ value: null, error: null });
  const [log, setLog] = React.useState({ value: null, error: null });
  const [commit, setCommit] = React.useState({ sha: null, value: null, error: null });
  const [file, setFile] = React.useState({ key: null, value: null, error: null });

  // The working-tree selection, saved when a commit is browsed and restored
  // when the working-tree row is clicked.
  const worktreeSelected = React.useRef(null);
  // The dirs holding changes open once, on the first status; later
  // refreshes keep what the user folded.
  const expandedInit = React.useRef(false);
  // Read by the status callback: it picks a working-tree file only while the
  // working tree is on screen.
  const commitShaRef = React.useRef(commitSha);
  commitShaRef.current = commitSha;
  // The commit whose first touched file was selected: a refresh of the same
  // commit keeps the selection.
  const commitPicked = React.useRef(null);

  React.useEffect(() => {
    let current = true;
    bridge.gitBrowserStatus().then(res => {
      if (!current) return;
      const r = _ugbResult(res);
      setStatus(r);
      const tree = r.value?.tree;
      if (!tree) return;
      if (!expandedInit.current) {
        expandedInit.current = true;
        const open = _UGB_changedDirs(tree);
        setExpanded(prev => new Set([...prev, ...open]));
      }
      // Nothing picked yet (no initial path, or a discard deleted it):
      // the first changed file.
      if (commitShaRef.current) return;
      setSelected(prev => prev ?? _UGB_firstFile(tree, true)?.path ?? null);
    });
    return () => { current = false; };
  }, [bridge, tick]);

  React.useEffect(() => {
    let current = true;
    bridge.gitBrowserLog().then(res => {
      if (current) setLog(_ugbResult(res));
    });
    return () => { current = false; };
  }, [bridge, logTick]);

  // Commit mode: the commit's touched files; the selection moves to the
  // first of them when the commit (not a refresh of it) arrives.
  React.useEffect(() => {
    if (!commitSha) return undefined;
    let current = true;
    bridge.gitBrowserCommit(commitSha).then(res => {
      if (!current) return;
      const r = _ugbResult(res);
      setCommit({ sha: commitSha, ...r });
      if (r.value && commitPicked.current !== commitSha) {
        commitPicked.current = commitSha;
        setSelected(_UGB_firstFile(r.value.tree, false)?.path ?? null);
      }
    });
    return () => { current = false; };
  }, [bridge, commitSha]);

  // The file pane. Rust normalises the path (an edit card's absolute target
  // becomes project-relative): the selection takes the normalised one, and
  // `remapped` spares that rename its own refetch — only that one: it
  // carries the tick, so a refresh landing in the same batch still fetches.
  // A commit's file is fixed: only the working tree's follows the tick.
  const fileKey = selected ? `${commitSha ?? "@"}\0${selected}` : null;
  const fileTick = commitSha ? "" : tick;
  const remapped = React.useRef(null);
  React.useEffect(() => {
    if (!selected) return undefined;
    const skip = remapped.current === `${fileKey}\0${fileTick}`;
    remapped.current = null;
    if (skip) return undefined;
    let current = true;
    const req = commitSha ? bridge.gitBrowserCommitFile(commitSha, selected) : bridge.gitBrowserFile(selected);
    req.then(res => {
      if (!current) return;
      const r = _ugbResult(res);
      const path = r.value?.path;
      if (path && path !== selected) {
        const key = `${commitSha ?? "@"}\0${path}`;
        remapped.current = `${key}\0${fileTick}`;
        setFile({ key, ...r });
        setSelected(path);
        setExpanded(prev => new Set([...prev, ..._UGB_ancestorDirs(path)]));
      } else {
        setFile({ key: fileKey, ...r });
      }
    });
    return () => { current = false; };
    // `fileKey` is `commitSha` + `selected`.
  }, [bridge, fileKey, fileTick]);

  const toggleDir = React.useCallback(path => setExpanded(prev => {
    const next = new Set(prev);
    if (next.has(path)) next.delete(path); else next.add(path);
    return next;
  }), []);

  /** A history row: a commit sha browses it, "@" returns to the working
   *  tree and its selection. Browsing starts with no file selected until
   *  the commit names its touched files. */
  const browse = React.useCallback(id => {
    if (id === "@") {
      if (!commitSha) return;
      commitPicked.current = null;
      setCommitSha(null);
      setSelected(worktreeSelected.current ?? _UGB_firstFile(status.value?.tree ?? null, true)?.path ?? null);
      return;
    }
    if (id === commitSha) return;
    if (!commitSha) worktreeSelected.current = selected;
    setSelected(null);
    setCommitSha(id);
  }, [commitSha, selected, status.value]);

  const stage = React.useCallback(async path => {
    await bridge.workspaceAccept(path);
    refresh();
  }, [bridge, refresh]);

  // A file with no HEAD blob is deleted, not restored: confirm first, and
  // let the next status pick another file.
  const discard = React.useCallback(async (path, kind) => {
    const deletes = _UGB_discardDeletes(kind);
    if (deletes && !window.confirm(`Delete ${path}? It has no committed version — this cannot be undone.`)) return;
    await bridge.workspaceReject(path);
    if (deletes) setSelected(null);
    refresh();
  }, [bridge, refresh]);

  const browsing = commitSha && commit.sha === commitSha ? commit : null;
  const shownFile = fileKey && file.key === fileKey ? file : null;
  // Commit mode shows the commit's touched files, the working tree its own.
  const shownTree = commitSha ? browsing?.value : status.value;
  return {
    status: status.value,
    log: log.value,
    commitSha,
    commit: browsing?.value ?? null,
    tree: shownTree?.tree ?? null,
    truncated: Boolean(shownTree?.truncated),
    // Status/log failures (e.g. not a git repository) replace the panes;
    // a commit or file failure shows in its own pane.
    error: status.error ?? log.error ?? null,
    commitError: browsing?.error ?? null,
    file: shownFile?.value ?? null,
    fileError: shownFile?.error ?? null,
    selected, expanded, changedOnly, filter,
    select: setSelected, toggleDir, setChangedOnly, setFilter, browse, refresh, stage, discard,
  };
}

Object.assign(window, { useGitBrowser });
