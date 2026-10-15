/* app/use-browser.jsx — the browser panel's state (docs/architecture/browser.md).
   Rust owns Chromium, the screencast and the annotation
   overlay (src-tauri/src/browser/); this hook tracks which tabs have the panel
   open, mirrors the `browser://` events of the open tab's page, and keeps the
   annotation chips. Painting and input forwarding are app/browser-screen.js,
   the pure decisions app/browser.js. The panel element must stay mounted
   while `open` (the effect binds the `viewRef`/`canvasRef`/`sinkRef`
   elements of the commit that opened it). */

const { OMP_BROWSER: _UB, OMP_BROWSER_SCREEN: _UBS } = window;
const _UB_IDLE = Object.freeze({ status: "idle", error: null, url: "", loading: false, canBack: false, canForward: false, annotate: false });
const _UB_NO_TABS = Object.freeze([]);
let _ubChipSeq = 0;

function useBrowser({ bridge, activeSessionId, sessionIds, tweaks, setTweak }) {
  const sid = activeSessionId || null;
  const [openBy, setOpenBy] = React.useState(_UB.EMPTY); // sid → true
  const [fullView, setFullView] = React.useState(false);
  const [state, setState] = React.useState(_UB_IDLE);
  const [tabs, setTabs] = React.useState(_UB_NO_TABS);
  const [page, setPage] = React.useState(null); // { cssW, cssH } of the shown page
  const [annotate, setAnnotateState] = React.useState(false);
  const [pendingAny, setPending] = React.useState(null); // { sid, pick, image }
  const [chips, setChips] = React.useState(_UB.EMPTY); // sid → chip[]
  const canvasRef = React.useRef(null);
  const viewRef = React.useRef(null);
  const sinkRef = React.useRef(null);
  const screenRef = React.useRef(null);
  const chipsRef = React.useRef(_UB.EMPTY);
  const open = Boolean(sid && openBy[sid]);
  const pending = pendingAny && pendingAny.sid === sid ? pendingAny : null;
  const wantUrl = React.useRef(null); // { sid, url } to navigate to once the panel is open
  // The active effect's open token; pending resizes stay local to that run.
  // A stale open completion must not release a newer run's queued work.
  const openInFlight = React.useRef(null);
  // What the key handlers (bound once per open) must see current.
  const latest = React.useRef({});
  latest.current = { annotate, pending };
  // Bumped whenever annotate is left, so a capture that settles after the
  // pick has been dismissed (Esc / annotate off / restart) is dropped.
  const pickGen = React.useRef(0);

  const updateChips = React.useCallback((fn) => {
    chipsRef.current = fn(chipsRef.current);
    setChips(chipsRef.current);
  }, []);

  // A closed tab drops its panel state and chips.
  const idsKey = (sessionIds ?? []).join("\0");
  React.useEffect(() => {
    const ids = idsKey ? idsKey.split("\0") : [];
    setOpenBy((s) => _UB.prune(s, ids));
    updateChips((s) => _UB.prune(s, ids));
    setPending((p) => (p && ids.includes(p.sid) ? p : null));
  }, [idsKey, updateChips]);

  // Annotate mode as shown; Rust's `state.annotate` is the truth and
  // overrides it (it also turns off when the agent acts on the page).
  const showAnnotate = React.useCallback((on) => {
    if (!on) pickGen.current += 1; // a pick made before this must not land
    setAnnotateState(on);
  }, []);
  const setAnnotate = React.useCallback((on) => {
    if (!sid) return;
    showAnnotate(on);
    bridge?.browserAnnotate(sid, on);
  }, [bridge, sid, showAnnotate]);

  // The open tab's panel: attach every listener, then open (the backend's
  // first `state` event fires during the open). Leaving (close, another tab)
  // stops the screencast; Chromium stays.
  React.useEffect(() => {
    const view = viewRef.current;
    const canvas = canvasRef.current;
    const sink = sinkRef.current;
    if (!bridge || !sid || !open || !view || !canvas || !sink) return undefined;
    let live = true;
    // While `browserOpenPanel` settles the view is not live: a resize is
    // queued here (latest wins) and a URL by `wantUrl`, both flushed once
    // the open resolves.
    const opening = { pending: true };
    openInFlight.current = opening;
    let pendingSize = null;
    setState(_UB_IDLE);
    setTabs(_UB_NO_TABS);
    setPage(null);
    const screen = _UBS.createScreen({
      view, canvas, sink,
      send: (ev) => bridge.browserInput(sid, ev),
      ack: (seq) => bridge.browserFrameAck(sid, seq),
      onResize: (w, h, dpr) => {
        if (opening.pending) pendingSize = { w, h, dpr };
        else bridge.browserResize(sid, w, h, dpr);
      },
      onPage: setPage,
      // Esc while annotating closes the comment popover, else leaves annotate.
      escape: () => _UB.escapeAction(latest.current.annotate, latest.current.pending),
      onEscape: (action) => (action === "cancelPick" ? setPending(null) : setAnnotate(false)),
    });
    screenRef.current = screen;
    // A pick stands only if annotate was not left before its capture
    // settled (`pickGen`). A failed capture still opens the popover:
    // `image: null` keeps the text flowing, the popover notes the failure.
    const onPick = async (pick) => {
      const gen = pickGen.current;
      const r = await bridge.browserCapture(sid, pick.docRect);
      const shot = r.ok ? { type: "image", data: r.value.data, mimeType: r.value.mimeType } : null;
      const image = shot && (await window.OMP_IMAGES.normalizeImage(shot).catch(() => null));
      if (!live || gen !== pickGen.current) return;
      setPending({ sid, pick, image });
    };
    const onState = (s) => {
      setState(s);
      const on = Boolean(s?.annotate);
      if (on !== latest.current.annotate) showAnnotate(on);
    };
    const offs = [
      bridge.onBrowserEvent(sid, "frame", screen.frame),
      bridge.onBrowserEvent(sid, "tabs", (p) => setTabs(p?.tabs ?? _UB_NO_TABS)),
      bridge.onBrowserEvent(sid, "state", onState),
      bridge.onBrowserEvent(sid, "pick", onPick),
    ];
    Promise.all(offs.map((o) => o.ready)).then(async () => {
      if (!live) return;
      const s = screen.size();
      try {
        await bridge.browserOpenPanel(sid, s.width, s.height, s.dpr);
      } finally {
        opening.pending = false;
      }
      if (!live) return;
      // Live now: the latest queued resize, then the latest queued URL (a
      // `/browser <url>` or localhost link asked for while Chromium started).
      if (pendingSize) bridge.browserResize(sid, pendingSize.w, pendingSize.h, pendingSize.dpr);
      const want = wantUrl.current;
      if (want?.sid === sid) {
        wantUrl.current = null;
        bridge.browserNavigate(sid, want.url);
      }
    });
    return () => {
      live = false;
      if (openInFlight.current === opening) openInFlight.current = null;
      for (const off of offs) off();
      screen.destroy();
      screenRef.current = null;
      bridge.browserClosePanel(sid);
      showAnnotate(false);
      setPending(null);
    };
  }, [bridge, sid, open, setAnnotate, showAnnotate]);

  const setOpen = React.useCallback((on) => {
    if (!sid) return;
    setOpenBy((s) => {
      if (on) return { ...s, [sid]: true };
      const { [sid]: _closed, ...rest } = s;
      return rest;
    });
    if (!on) setFullView(false);
  }, [sid]);
  const toggle = React.useCallback(() => setOpen(!open), [setOpen, open]);
  const close = React.useCallback(() => setOpen(false), [setOpen]);
  // Show `url` (already normalised) in the panel, opening it first if needed.
  // While `browserOpenPanel` is still in flight the backend has no view to
  // navigate yet, so the URL is queued (latest wins) and the open flushes it.
  const openUrl = React.useCallback((url) => {
    if (!sid) return;
    if (open && !openInFlight.current?.pending) {
      bridge?.browserNavigate(sid, url);
      return;
    }
    wantUrl.current = { sid, url };
    if (!open) setOpen(true);
  }, [bridge, sid, open, setOpen]);

  // A plain click on a localhost link in the chat opens it here (when the
  // tweak is on); a modifier click opens it in the system browser through
  // the external opener — wry has no `on_new_window` handler, so the native
  // new-window behaviour would open nothing.
  const localLinks = tweaks?.browserLocalhostInPanel ?? true;
  const openLink = React.useRef(null);
  openLink.current = (url, external) => {
    if (!external) return openUrl(url);
    Promise.resolve(bridge?.openExternalUrl(url)).then(
      (res) => { if (res && !res.ok) console.error("[browser] open failed:", res.error); },
      (err) => console.error("[browser] open failed:", err),
    );
  };
  React.useEffect(
    () => (localLinks && sid ? _UBS.watchLocalLinks((url, external) => openLink.current(url, external)) : undefined),
    [localLinks, sid],
  );

  const savePending = React.useCallback((comment) => {
    const text = comment.trim();
    if (!pending || !text) return;
    const chip = { id: `bc${++_ubChipSeq}`, pick: pending.pick, comment: text, image: pending.image };
    updateChips((s) => _UB.addChip(s, pending.sid, chip));
    setPending(null);
  }, [pending, updateChips]);
  const cancelPending = React.useCallback(() => setPending(null), []);
  // `id` defaults to the tab shown; a send's callbacks keep the tab of the
  // render that sent it, so a reply after a tab switch hits the right tab.
  // Reserve: a submit takes its chips out of circulation at once. Spend:
  // delivered, gone (chips added meanwhile stay). Release: refused or
  // timed out, back for the next send.
  const chipActions = React.useMemo(() => {
    const on = (reduce) => (ids, id = sid) => updateChips((s) => reduce(s, id, ids));
    return {
      removeChip: (id) => updateChips((s) => _UB.removeChip(s, sid, id)),
      spendChips: on(_UB.removeChips),
      reserveChips: on(_UB.reserveChips),
      releaseChips: on(_UB.releaseChips),
    };
  }, [sid, updateChips]);

  // Page actions on the open tab; a refused navigation shows in the page.
  const act = React.useMemo(() => ({
    navigate: (url) => bridge?.browserNavigate(sid, url),
    history: (delta) => bridge?.browserHistory(sid, delta),
    newTab: () => bridge?.browserTabNew(sid),
    closeTab: (id) => bridge?.browserTabClose(sid, id),
    activateTab: (id) => bridge?.browserTabActivate(sid, id),
    // After a crash the screencast is gone: open the panel again.
    restart: () => {
      showAnnotate(false);
      setPending(null);
      const s = screenRef.current?.size();
      if (s) bridge?.browserOpenPanel(sid, s.width, s.height, s.dpr);
    },
  }), [bridge, sid, showAnnotate]);

  const chipsList = _UB.chipsOf(chips, sid);
  // Reserved chips are out of circulation while their send settles: out of
  // every merge (`sendableChips`) and of the composer's sendable count; the
  // strip still shows them, dimmed, from the full list.
  const sendableChips = chipsList.filter((c) => !c.reserved);
  return {
    open, toggle, close, openUrl, fullView, setFullView,
    width: tweaks?.browserWidth ?? 0, // px; 0 = default (half the stage) — see OMP_BROWSER.clampWidth
    setWidth: (px) => setTweak("browserWidth", Math.round(px)),
    state, tabs, page, annotate, setAnnotate,
    pending, savePending, cancelPending,
    chips: chipsList, sendableChips, sendableCount: sendableChips.length,
    ...chipActions, canvasRef, viewRef, sinkRef, ...act,
  };
}

window.useBrowser = useBrowser;
