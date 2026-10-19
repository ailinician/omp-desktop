/* app/use-remote.jsx — publish the tab list while the server listens and the
   active tab's view only while a phone is connected.
   Rust owns transport/auth; existing bridge methods execute phone actions. */
function useRemote({ bridge }) {
  React.useEffect(() => {
    if (!bridge) return undefined;
    const { tabMeta, buildView, viewsEqual, makeActionHandler } = window.OMP_REMOTE_VIEW;
    let live = true, snapshot = null, status = null, statusSeen = false;
    let timer = null, last = null, publishedAt = -Infinity, inflight = false, dirty = false, epoch = 0;
    const unlisten = [];
    const wanted = () => live && Boolean(status?.listening);
    const wantsView = () => wanted() && status.devices?.some(device => device.connected);

    function themePayload() {
      const defaults = window.TWEAK_DEFAULTS ?? {};
      const t = window.OMP_TWEAKS_STORE?.read(defaults) ?? defaults;
      const theme = window.OMP_SETTINGS?.THEMES.find(th => th.id === t.theme) ?? { tone: "dark" };
      return { id: t.theme ?? "aurora", tone: theme.tone ?? "dark", accent: t.accent };
    }

    function publish() {
      timer = null;
      if (!wanted() || !snapshot) return;
      if (inflight) { dirty = true; return; }
      const payload = { tabs: tabMeta(snapshot.sessions), view: wantsView() ? buildView(snapshot) : null, theme: themePayload() };
      if (last && viewsEqual(last.view, payload.view) && viewsEqual(last.tabs, payload.tabs) && viewsEqual(last.theme, payload.theme)) return;
      publishedAt = Date.now();
      inflight = true;
      const sent = epoch;
      // Bridge returns errors as data, never logs streaming publish failures.
      // `last` only advances once Rust took the payload, so a failure retries.
      Promise.resolve(bridge.remotePublish(payload)).then(result => {
        if (epoch === sent && result?.ok !== false) last = payload;
      }, () => {}).finally(() => {
        inflight = false;
        // A failed publish retries on the next change, not in a loop.
        if (dirty) { dirty = false; schedule(); }
      });
    }
    function schedule() {
      if (!wanted() || !snapshot || timer !== null) return;
      if (inflight) { dirty = true; return; }
      timer = setTimeout(publish, Math.max(0, 250 - (Date.now() - publishedAt)));
    }
    function applyStatus(value) {
      if (!live) return;
      status = value;
      if (!wanted()) {
        clearTimeout(timer);
        timer = null;
        last = null;
        epoch++;
      } else schedule();
    }
    function listen(kind, handler) {
      return Promise.resolve(bridge.onRemoteEvent(kind, payload => { if (live) handler(payload); }))
        .then(stop => { if (live) unlisten.push(stop); else stop(); });
    }

    const handle = makeActionHandler(bridge, () => live ? snapshot : null, () => live ? status : null);
    const onTweakChange = schedule;
    window.addEventListener("tweakchange", onTweakChange);
    unlisten.push(() => window.removeEventListener("tweakchange", onTweakChange));
    unlisten.push(bridge.onUpdate(value => { snapshot = value; schedule(); }));
    const listening = Promise.all([
      listen("status", value => { statusSeen = true; applyStatus(value); }),
      listen("watch", schedule),
      listen("action", action => { void handle(action).catch(() => {}); }),
    ]);
    // Ready only once the listeners exist: Rust resets readiness on page load
    // and must not hand out actions or views before they can be received.
    // Fetch after subscribing, and never overwrite a newer status event.
    void listening.then(() => {
      if (live) void bridge.remoteReady(true);
      return bridge.remoteStatus();
    }).then(value => {
      if (!statusSeen && value) applyStatus(value);
    }).catch(() => {});

    return () => {
      live = false;
      clearTimeout(timer);
      for (const stop of unlisten) stop();
      void bridge.remoteReady(false);
    };
  }, [bridge]);
}

window.useRemote = useRemote;
