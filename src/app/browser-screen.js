// app/browser-screen.js — DOM side of the browser panel (docs/architecture/browser.md):
// paints the latest screencast frame on a canvas and
// forwards the user's pointer, wheel, key, text and paste input to the page.
//   createScreen({ view, canvas, sink, send, ack, onResize, onPage, escape, onEscape })
//     -> { frame(f), size(), destroy() }
//   watchLocalLinks(open) -> unsubscribe
// `view` is the wrapper the canvas fills; `sink` a hidden <textarea> inside
// it that holds keyboard focus for the page, so paste, IME composition and
// dead keys work (an editable target): keys that type nothing go out as key
// events, composed text and pastes as `{ kind: "text" }`. `send(ev)` takes
// an `InputEvent` (app/browser.js). `ack(seq)` tells Rust a frame was
// painted or dropped (one frame in flight per view). `onResize(w, h, dpr)`
// fires 100 ms after the view's CSS size or the display's pixel ratio
// settled on a new value; `onPage({cssW, cssH})` when the painted frame's
// CSS size changes. `escape()` is what Esc on the panel does now
// (OMP_BROWSER.escapeAction, null = the page's key), `onEscape(action)`
// does it. Chords the app keymap takes leave the page the way they leave
// the terminal (OMP_KEYMAP.guestShouldBypass). Pure mappings live in app/browser.js;
// DOM scheduling and painting have a minimal-stub regression in
// tests/test-browser-runtime.mjs.
(function () {
  const RESIZE_MS = 100;
  const LINE_PX = 16;

  function createScreen({ view, canvas, sink, send, ack, onResize, onPage, escape, onEscape }) {
    const B = window.OMP_BROWSER;
    const bytesOf = window.OMP_IMAGES.b64Bytes;
    const ctx = canvas.getContext("2d");
    let dead = false;
    let last = null; // newest frame received: its CSS size maps the clicks
    let incoming = null; // newest frame not yet decoded
    let decoding = false;
    let ready = null; // decoded, not yet painted: { bmp, cssW, cssH, seq }
    let shown = null; // painted: { bmp, cssW, cssH }; kept to redraw after a resize
    let paintRaf = 0;
    let published = null; // geometry sent with the last painted frame
    let moveRaf = 0;
    let move = null;
    let pressed = false;
    let resizeTimer = 0;
    let dprQuery = null;

    // Every frame Rust sent is acked exactly once: painted, or dropped
    // (superseded before decode or paint, undecodable).
    const done = (f) => { if (f && f.seq != null) ack(f.seq); };

    const dpr = () => window.devicePixelRatio || 1;
    const sizeNow = () => {
      const r = view.getBoundingClientRect();
      return { width: Math.round(r.width), height: Math.round(r.height), dpr: dpr() };
    };
    let sent = sizeNow();

    // Canvas backing store = view CSS size × dpr (assigning clears it).
    function fit() {
      const r = view.getBoundingClientRect();
      const w = Math.max(1, Math.round(r.width * dpr()));
      const h = Math.max(1, Math.round(r.height * dpr()));
      if (canvas.width !== w) canvas.width = w;
      if (canvas.height !== h) canvas.height = h;
    }

    function paint() {
      paintRaf = 0;
      if (dead) return;
      fit();
      let painted = null;
      if (ready) {
        shown?.bmp.close();
        painted = ready;
        shown = { bmp: ready.bmp, cssW: ready.cssW, cssH: ready.cssH };
        ready = null;
      }
      if (shown) {
        ctx.drawImage(shown.bmp, 0, 0, canvas.width, canvas.height);
        // Geometry goes out with the bitmap actually painted, not when the
        // frame arrived: markers map what is on screen.
        if (!published || published.cssW !== shown.cssW || published.cssH !== shown.cssH) {
          published = { cssW: shown.cssW, cssH: shown.cssH };
          onPage(published);
        }
      }
      done(painted);
    }
    const schedule = () => { if (!paintRaf) paintRaf = requestAnimationFrame(paint); };

    // Decode only the newest frame; frames that arrive meanwhile replace
    // each other, and a decoded one that was never painted is closed.
    async function pump() {
      if (decoding) return;
      decoding = true;
      while (incoming && !dead) {
        const f = incoming;
        incoming = null;
        try {
          const bmp = await createImageBitmap(new Blob([bytesOf(f.b64)], { type: "image/jpeg" }));
          if (dead) { bmp.close(); break; }
          if (ready) { ready.bmp.close(); done(ready); }
          ready = { bmp, cssW: f.cssW, cssH: f.cssH, seq: f.seq };
          schedule();
        } catch {
          done(f); // an undecodable frame is dropped; the next one replaces it
        }
      }
      decoding = false;
    }

    function frame(f) {
      if (dead || !f?.b64) return;
      last = f;
      done(incoming);
      incoming = f;
      void pump();
    }

    // A size reaches Rust once it held for RESIZE_MS: the view's CSS size
    // (ResizeObserver) or the display's pixel ratio (moving the window to a
    // monitor with another scale changes no CSS size).
    function settle() {
      schedule();
      clearTimeout(resizeTimer);
      resizeTimer = setTimeout(() => {
        const s = sizeNow();
        if (dead || !s.width || !s.height) return;
        if (s.width === sent.width && s.height === sent.height && s.dpr === sent.dpr) return;
        sent = s;
        onResize(s.width, s.height, s.dpr);
      }, RESIZE_MS);
    }
    const ro = new ResizeObserver(settle);
    ro.observe(view);
    // A `(resolution: Ndppx)` query matches only the current ratio, so it is
    // re-armed for the new one on every change.
    function watchDpr() {
      dprQuery?.removeEventListener("change", onDpr);
      dprQuery = window.matchMedia?.(`(resolution: ${dpr()}dppx)`) ?? null;
      dprQuery?.addEventListener("change", onDpr);
    }
    function onDpr() {
      if (dead) return;
      watchDpr();
      settle();
    }
    watchDpr();

    // ---- input ---------------------------------------------------------------

    const at = (e) => (last ? B.toPageCoords(e, view.getBoundingClientRect(), last) : null);
    const buttonOf = (e) => (e.buttons & 1 ? "left" : e.buttons & 4 ? "middle" : e.buttons & 2 ? "right" : "none");

    function mouse(type, e, button) {
      const p = at(e);
      if (!p) return;
      send({ kind: "mouse", type, x: p.x, y: p.y, button, buttons: e.buttons, clickCount: type === "mouseMoved" ? 0 : Math.max(1, e.detail), modifiers: B.modifiersOf(e) });
    }

    // The coalesced move still in its rAF belongs to the gesture in flight;
    // it goes out before a release or a new press, so a fast drag stays an
    // area pick and a pressed move never follows the release.
    function flushMove() {
      if (!moveRaf) return;
      cancelAnimationFrame(moveRaf);
      moveRaf = 0;
      if (move && !dead) send(move);
      move = null;
    }

    const onDown = (e) => {
      if (e.target !== canvas) return;
      e.preventDefault(); // keeps the page selection ours, so focus is taken by hand
      sink.focus({ preventScroll: true });
      pressed = true;
      flushMove();
      mouse("mousePressed", e, B.mouseButton(e.button));
    };
    const onUp = (e) => {
      if (!pressed) return;
      pressed = false;
      flushMove();
      mouse("mouseReleased", e, B.mouseButton(e.button));
    };
    // Moves are coalesced to one per frame; a drag keeps reporting after the
    // pointer left the view.
    const onMove = (e) => {
      if (!pressed && e.target !== canvas) return;
      const p = at(e);
      if (!p) return;
      move = { kind: "mouse", type: "mouseMoved", x: p.x, y: p.y, button: buttonOf(e), buttons: e.buttons, clickCount: 0, modifiers: B.modifiersOf(e) };
      if (!moveRaf) {
        moveRaf = requestAnimationFrame(() => {
          moveRaf = 0;
          if (move && !dead) send(move);
          move = null;
        });
      }
    };
    const onWheel = (e) => {
      if (e.target !== canvas) return;
      e.preventDefault();
      const p = at(e);
      if (!p) return;
      const unit = e.deltaMode === 1 ? LINE_PX : e.deltaMode === 2 ? view.clientHeight : 1;
      send({ kind: "wheel", x: p.x, y: p.y, deltaX: e.deltaX * unit, deltaY: e.deltaY * unit, modifiers: B.modifiersOf(e) });
    };
    const onContext = (e) => { if (e.target === canvas) e.preventDefault(); };

    // Only keys typed into the sink (not in the popover or toolbar). A key
    // the IME or a dead key is working on stays in the sink: its result
    // arrives as text. Ctrl/⌘+V pastes natively into the sink (WebKit only
    // enables paste on an editable target) and reaches the page as text.
    const composing = (e) => e.isComposing || e.key === "Process" || e.key === "Dead";
    const isPaste = (e) => (e.ctrlKey || e.metaKey) && e.key.toLowerCase() === "v";
    const appKey = (e) => (e.key === "Escape" && escape() !== null) || window.OMP_KEYMAP.guestShouldBypass(e);
    const onKey = (type) => (e) => {
      if (e.target !== sink || composing(e) || isPaste(e) || appKey(e)) return;
      e.preventDefault();
      send(B.keyEventToCdp(e, type));
    };
    const onKeyDown = onKey("keyDown");
    const onKeyUp = onKey("keyUp");
    // Whatever text landed in the sink (a finished composition, a dead-key
    // character) goes to the page once and the sink is emptied.
    const flushText = () => {
      const text = sink.value;
      sink.value = "";
      if (text) send({ kind: "text", text });
    };
    const onInput = (e) => { if (!e.isComposing) flushText(); };
    const onPaste = (e) => {
      const text = e.clipboardData?.getData("text/plain");
      e.preventDefault();
      if (text) send({ kind: "text", text });
    };

    // Esc on the panel while annotating is the panel's: never the page's,
    // never the app keymap's interrupt (which ignores a prevented key).
    // Bound on the panel, so Esc in the composer is not ours.
    const panel = view.closest?.(".browser-panel") ?? view;
    const onEsc = (e) => {
      const action = e.key === "Escape" ? escape() : null;
      if (!action) return;
      e.preventDefault();
      onEscape(action);
    };

    const on = [
      [panel, "keydown", onEsc], [view, "mousedown", onDown], [window, "mouseup", onUp], [window, "mousemove", onMove],
      [view, "wheel", onWheel, { passive: false }], [view, "contextmenu", onContext],
      [sink, "keydown", onKeyDown], [sink, "keyup", onKeyUp], [sink, "paste", onPaste],
      [sink, "input", onInput], [sink, "compositionend", flushText],
    ];
    for (const [t, type, fn, opt] of on) t.addEventListener(type, fn, opt);

    function destroy() {
      dead = true;
      for (const [t, type, fn, opt] of on) t.removeEventListener(type, fn, opt);
      ro.disconnect();
      dprQuery?.removeEventListener("change", onDpr);
      clearTimeout(resizeTimer);
      cancelAnimationFrame(paintRaf);
      cancelAnimationFrame(moveRaf);
      ready?.bmp.close();
      shown?.bmp.close();
      ready = shown = null;
    }

    return { frame, size: sizeNow, destroy };
  }

  // Primary clicks on localhost links in the chat (OMP_BROWSER.linkClick)
  // go to `open(url, external)` instead of the webview. Returns the
  // unsubscribe.
  function watchLocalLinks(open) {
    const onClick = (e) => {
      const link = window.OMP_BROWSER.linkClick(e);
      if (!link) return;
      e.preventDefault();
      open(link.url, link.external);
    };
    document.addEventListener("click", onClick);
    return () => document.removeEventListener("click", onClick);
  }

  window.OMP_BROWSER_SCREEN = { createScreen, watchLocalLinks };
})();
