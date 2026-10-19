// Standalone phone UI. Only configured markdown enters innerHTML; all other
// remote data is textContent. No desktop bridge or external resources.
(function () {
  const C = window.OMP_REMOTE_CLIENT;
  const TOKEN_KEY = "omp-remote-token";
  let state = { device: null, tabs: [], view: null, pending: {}, error: null, theme: null };
  // A push tap opens `/?tab=<id>`; that id is used for the first connection only.
  let socket, token, selected = null, pendingTab = new URLSearchParams(location.search).get("tab");
  if (pendingTab) history.replaceState(null, "", location.pathname + location.hash);
  const openTools = new Set();
  let connected = false, attempt = 0, notificationsEnabled = false;
  let connectionText = "Connecting…";
  let renderedAsks = null;
  const askDrafts = new Map();

  function node(tag, text, className) {
    const element = document.createElement(tag);
    if (text !== undefined) element.textContent = text;
    if (className) element.className = className;
    return element;
  }
  function button(label, click) {
    const element = node("button", label);
    element.type = "button";
    element.addEventListener("click", click);
    return element;
  }
  const app = document.getElementById("app");
  // Fixed header: the active tab, a count of other tabs with questions and the
  // connection. Tapping it opens the session sheet.
  const topDot = node("span", "", "tab-run-dot");
  const topTitle = node("span", "OMP Remote", "top-title");
  const topProject = node("span", "", "top-project");
  topProject.hidden = true;
  const topText = node("span", undefined, "top-text");
  topText.append(topTitle, topProject);
  const topCount = node("span", "", "top-count");
  topCount.hidden = true;
  const topCaret = node("span", "▾", "top-caret");
  const connDot = node("span", "", "conn-dot");
  const header = node("button", undefined, "top");
  header.type = "button";
  header.setAttribute("aria-haspopup", "dialog");
  header.setAttribute("aria-expanded", "false");
  header.append(topDot, topText, topCount, topCaret, connDot);
  header.addEventListener("click", () => (sheetOpen ? closeSheet() : openSheet()));
  const status = node("p", "", "status");
  status.setAttribute("role", "status");
  const error = node("p", "", "error");
  error.setAttribute("role", "alert");
  // Bottom sheet listing every session; footer holds the app label, the
  // connection and the notifications control.
  const backdrop = node("div", undefined, "sheet-backdrop");
  backdrop.hidden = true;
  backdrop.addEventListener("click", () => closeSheet());
  const sheet = node("section", undefined, "sheet");
  sheet.hidden = true;
  sheet.setAttribute("role", "dialog");
  sheet.setAttribute("aria-modal", "true");
  sheet.setAttribute("aria-label", "Sessions");
  sheet.setAttribute("tabindex", "-1");
  const handle = node("button", undefined, "sheet-handle");
  handle.type = "button";
  handle.setAttribute("aria-label", "Close sessions");
  handle.append(node("span", "", "sheet-grip"));
  const sheetList = node("div", undefined, "sheet-list");
  const notifications = button("Enable notifications", enableNotifications);
  const sheetConn = node("span", "", "sheet-conn");
  sheetConn.setAttribute("role", "status");
  const sheetFoot = node("footer", undefined, "sheet-foot");
  const sheetInfo = node("div", undefined, "sheet-info");
  sheetInfo.append(node("strong", "OMP Remote"), sheetConn);
  sheetFoot.append(sheetInfo, notifications);
  sheet.append(handle, sheetList, sheetFoot);
  const transcript = node("section");
  transcript.setAttribute("aria-label", "Transcript");
  const asks = node("section");
  asks.setAttribute("aria-label", "Open questions");
  const scan = node("section", undefined, "scan");
  scan.setAttribute("aria-label", "Pair with camera");
  const scanButton = button("Pair with your desktop", startScan);
  const cancelScan = button("Cancel", () => { stopScan(); scanHint.textContent = ""; });
  cancelScan.hidden = true;
  const video = node("video");
  video.muted = true;
  video.setAttribute("playsinline", "");
  video.setAttribute("autoplay", "");
  video.hidden = true;
  const canvas = document.createElement("canvas");
  const scanHint = node("p", "", "scan-hint");
  scan.append(scanButton, cancelScan, video, scanHint);
  const composer = node("div", "", "remote-composer");
  const draft = node("textarea", undefined, "composer-field");
  draft.id = "draft";
  draft.rows = 1;
  draft.setAttribute("aria-label", "Message");
  draft.setAttribute("enterkeyhint", "enter");
  const send = button("↑", onComposerButton);
  send.className = "composer-btn";
  composer.append(draft, send);
  const jumpLatest = button("↓ Newest", () => { pinned = true; jumpToBottom(); updateJump(); });
  jumpLatest.className = "jump-latest";
  jumpLatest.hidden = true;
  const scrollBox = node("div", undefined, "scroll");
  scrollBox.append(status, scan, error, transcript, asks);
  const stage = node("div", undefined, "stage");
  stage.append(scrollBox, jumpLatest);
  app.append(header, stage, composer, backdrop, sheet);

  const P = window.OMP_SCROLL_PIN;
  let pinned = true, prevScroll = 0, lastViewTabId = null, lastTailId = null;
  // Ids already on screen for the current tab; only new ones fade in, so a
  // streaming rebuild does not make the whole transcript blink.
  let shownIds = new Set(), freshIds = new Set(), shownTab = null;
  const fade = key => (freshIds.has(key) ? " fade-up" : "");
  const installed = () => Boolean(navigator.standalone) || window.matchMedia("(display-mode: standalone)").matches;
  function scroller() { return scrollBox; }
  function measure() { const m = scroller(); return { scrollTop: m.scrollTop, scrollHeight: m.scrollHeight, clientHeight: m.clientHeight }; }
  function jumpToBottom() { const m = scroller(); m.scrollTop = m.scrollHeight - m.clientHeight; }
  function updateJump() { jumpLatest.hidden = pinned; }
  function onScroll() { const m = measure(); pinned = P.nextPinned(pinned, prevScroll, m); prevScroll = m.scrollTop; updateJump(); }
  function afterRender(keep) {
    const m = measure();
    const messages = state.view?.messages || [];
    const tail = messages.at(-1);
    const repin = C.repinFor(P, { tabId: lastViewTabId, lastId: lastTailId }, state.view);
    lastViewTabId = state.view?.tabId ?? null;
    lastTailId = tail?.id ?? null;
    const result = P.afterOutput({ pinned, follow: true, repin }, m);
    pinned = result.pinned;
    if (result.stick) jumpToBottom();
    else scroller().scrollTop = keep;
    prevScroll = measure().scrollTop;
    updateJump();
  }
  scrollBox.addEventListener("scroll", onScroll);
  // The box shrinks under the iOS keyboard, a growing draft or a rotation:
  // a pinned reader stays on the newest message (as desktop chat-view).
  if (typeof ResizeObserver !== "undefined") {
    new ResizeObserver(() => { if (pinned) { jumpToBottom(); prevScroll = measure().scrollTop; } }).observe(scrollBox);
  }

  function transmit(frame) {
    if (!connected || socket?.readyState !== WebSocket.OPEN) return false;
    socket.send(JSON.stringify(frame));
    return true;
  }
  function action(kind, fields = {}) {
    if (!state.view || !connected) return false;
    const frame = C.actionFor(kind, { tabId: state.view.tabId, ...fields });
    if (!transmit(frame)) return false;
    state = { ...state, pending: { ...state.pending, [frame.reqId]: kind }, error: null };
    updateControls();
    return true;
  }
  const currentAction = () => C.composerAction({ busy: Boolean(state.view?.busy), draft: draft.value });
  function onComposerButton() {
    if (currentAction() === "stop") action("abort");
    else submit();
  }
  // Grows with the text; the CSS max-height makes it scroll beyond ~5 lines.
  function fitDraft() {
    draft.style.height = "auto";
    draft.style.height = `${(draft.scrollHeight || 0) + (draft.offsetHeight - draft.clientHeight || 0)}px`;
  }
  function syncComposer() {
    const kind = currentAction();
    draft.placeholder = state.view?.busy ? "Steer…" : "Message…";
    send.textContent = kind === "stop" ? "■" : "↑";
    send.className = `composer-btn ${kind}`;
    send.setAttribute("aria-label", kind === "stop" ? "Stop" : kind === "steer" ? "Steer" : "Send");
    send.disabled = !connected || !state.view || (kind === "send" && !draft.value.trim());
    draft.disabled = !connected || !state.view;
  }
  draft.addEventListener("input", () => { fitDraft(); syncComposer(); });
  function submit() {
    if (!draft.value.trim()) return;
    if (action(state.view?.busy ? "steer" : "prompt", { text: draft.value })) { draft.value = ""; fitDraft(); updateControls(); }
  }
  function updateControls() {
    status.textContent = connectionText;
    status.hidden = connected;
    sheetConn.textContent = connectionText;
    connDot.className = `conn-dot ${connected ? "ok" : "off"}`;
    connDot.title = connected ? "Connected" : "Reconnecting";
    error.textContent = state.error || "";
    syncComposer();
    notifications.hidden = !(navigator.standalone || window.matchMedia("(display-mode: standalone)").matches)
      || !("PushManager" in window) || !("serviceWorker" in navigator);
    notifications.disabled = !connected || !state.pushKey || notificationsEnabled;
    for (const element of asks.querySelectorAll?.("button, input, textarea") || []) element.disabled = !connected;
    const canPair = !token && !connected;
    scan.hidden = !canPair;
    scanButton.hidden = !canPair || !video.hidden;
    if (!canPair) { stopScan(); scanHint.textContent = ""; }
  }

  // Every stopScan bumps the generation; an await that resumes under an older
  // one belongs to a cancelled scan and must release what it got and stop.
  let scanStream = null, scanTimer = null, scanGen = 0;
  function stopScan() {
    scanGen++;
    if (scanTimer) { clearInterval(scanTimer); scanTimer = null; }
    if (scanStream) { for (const track of scanStream.getTracks()) track.stop(); scanStream = null; }
    video.pause(); video.srcObject = null; video.hidden = true;
    cancelScan.hidden = true;
    scanButton.hidden = scan.hidden;
  }
  document.addEventListener("visibilitychange", () => { if (document.hidden) stopScan(); });
  window.addEventListener("pagehide", stopScan);
  async function startScan() {
    if (!navigator.mediaDevices?.getUserMedia) {
      scanHint.textContent = "Camera access is not available on this device.";
      return;
    }
    stopScan(); // a second tap replaces the running scan instead of leaking it
    const gen = scanGen;
    scanButton.hidden = true; cancelScan.hidden = false; video.hidden = false; scanHint.textContent = "Point the camera at the pairing QR code…";
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ video: { facingMode: "environment" } });
      if (gen !== scanGen) { for (const track of stream.getTracks()) track.stop(); return; }
      scanStream = stream;
      video.srcObject = stream;
      await video.play();
      if (gen !== scanGen) return; // stopScan already released the stream
      canvas.width = video.videoWidth || 640;
      canvas.height = video.videoHeight || 480;
      scanTimer = setInterval(() => {
        if (video.readyState !== video.HAVE_ENOUGH_DATA || !window.jsQR) return;
        const ctx = canvas.getContext("2d");
        ctx.drawImage(video, 0, 0, canvas.width, canvas.height);
        const image = ctx.getImageData(0, 0, canvas.width, canvas.height);
        const result = window.jsQR(image.data, image.width, image.height, { inversionAttempts: "dontInvert" });
        if (!result?.data) return;
        const parsed = C.pairCodeFromScan(result.data, location.origin);
        if (parsed?.code) {
          stopScan();
          pairFromCode(parsed.code);
        } else if (parsed?.other) {
          scanHint.textContent = "This code is for another server.";
        }
      }, 125);
    } catch (err) {
      if (gen !== scanGen) return; // cancelled: not a denial
      stopScan();
      scanHint.textContent = installed()
        ? "Camera access was denied. Allow camera access for this app in the iOS Settings, then try again."
        : "Camera access was denied. Allow camera access for this site in your browser settings, then try again.";
    }
  }

  async function pairFromCode(code) {
    try {
      const response = await fetch("/pair", { method: "POST", headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code, name: navigator.userAgent.slice(0, 120) }) });
      if (!response.ok) throw new Error(`Pairing failed (${response.status}); create a new pairing code on the desktop`);
      const paired = await response.json();
      token = paired.token;
      localStorage.setItem(TOKEN_KEY, token);
      scanHint.textContent = "";
      updateControls();
      connect();
    } catch (err) {
      scanHint.textContent = err.message;
      updateControls();
    }
  }

  function applyTheme(theme) {
    if (!theme) return;
    window.OMP_ACCENT?.applyThemeTokens(document.documentElement, theme.id, theme.accent);
  }
  applyTheme({ id: "aurora", tone: "dark", accent: "#8AF0C8" });

  const activeTabId = () => selected ?? state.view?.tabId ?? null;
  // Dot like the desktop tab's run dot: accent pulse while running, amber
  // "?" while a question waits, nothing visible when idle.
  function setDot(dot, tab) {
    const kind = tab?.hasAsk ? "waiting-user question" : tab?.busy ? "running" : "idle";
    dot.className = `tab-run-dot ${kind}`;
    dot.textContent = tab?.hasAsk ? "?" : "";
    dot.title = tab?.hasAsk ? "waiting for you" : tab?.busy ? "running" : "";
  }
  let sheetOpen = false, sheetSignature = null;
  function renderTabs() {
    const activeId = activeTabId();
    const active = state.tabs.find(tab => tab.id === activeId);
    const title = active ? active.title || "Untitled" : state.tabs.length ? "Select a session" : "OMP Remote";
    const project = C.projectName(active?.project);
    const others = C.otherQuestionCount(state.tabs, activeId);
    topTitle.textContent = title;
    topProject.textContent = project;
    topProject.hidden = !project;
    topCount.textContent = String(others);
    topCount.hidden = others === 0;
    setDot(topDot, active);
    header.setAttribute("aria-label", `${title}${project ? `, ${project}` : ""}${others ? `, ${others} other ${others === 1 ? "session has a question" : "sessions have questions"}` : ""}. Switch session`);
    // The sheet list is rebuilt only when it changed, so focus survives updates.
    const signature = JSON.stringify([activeId, state.tabs]);
    if (signature === sheetSignature) return;
    sheetSignature = signature;
    sheetList.replaceChildren();
    if (!state.tabs.length) sheetList.append(node("p", "No sessions open on the desktop.", "status"));
    for (const tab of state.tabs) {
      const item = button("", () => {
        if (!transmit(C.selectFor(tab.id))) return;
        selected = tab.id;
        // The same tab keeps its view: no new view frame is coming for it.
        if (tab.id !== state.view?.tabId) state = { ...state, view: null };
        closeSheet(); renderTabs(); renderView(); updateControls();
      });
      item.className = `sheet-tab${tab.id === activeId ? " active" : ""}`;
      if (tab.id === activeId) item.setAttribute("aria-current", "true");
      const dot = node("span", "", "tab-run-dot");
      setDot(dot, tab);
      const text = node("span", undefined, "sheet-tab-text");
      text.append(node("span", tab.title || "Untitled", "sheet-tab-title"));
      const folder = C.projectName(tab.project);
      if (folder) text.append(node("span", folder, "project"));
      item.append(dot, text);
      sheetList.append(item);
    }
  }
  function openSheet() {
    sheetOpen = true;
    backdrop.hidden = false;
    sheet.hidden = false;
    header.setAttribute("aria-expanded", "true");
    sheet.focus();
  }
  function closeSheet() {
    if (!sheetOpen) return;
    sheetOpen = false;
    backdrop.hidden = true;
    sheet.hidden = true;
    sheet.style.transform = "";
    header.setAttribute("aria-expanded", "false");
    header.focus();
  }
  // Swipe the handle down to close; a short drag springs back.
  let dragFrom = null, dragged = false;
  handle.addEventListener("pointerdown", event => {
    dragFrom = event.clientY; dragged = false;
    handle.setPointerCapture?.(event.pointerId);
  });
  handle.addEventListener("pointermove", event => {
    if (dragFrom === null) return;
    const dy = Math.max(0, event.clientY - dragFrom);
    if (dy > 8) dragged = true;
    sheet.style.transform = `translateY(${dy}px)`;
  });
  const endDrag = event => {
    if (dragFrom === null) return;
    const dy = event.clientY - dragFrom;
    dragFrom = null;
    sheet.style.transform = "";
    if (dy > 60) closeSheet();
  };
  handle.addEventListener("pointerup", endDrag);
  handle.addEventListener("pointercancel", endDrag);
  handle.addEventListener("click", () => { if (!dragged) closeSheet(); });
  document.addEventListener("keydown", event => {
    if (!sheetOpen) return;
    if (event.key === "Escape") { event.preventDefault(); closeSheet(); return; }
    if (event.key !== "Tab") return;
    // Keep focus inside the modal sheet.
    const items = [...(sheet.querySelectorAll?.("button") || [])].filter(el => !el.hidden && !el.disabled);
    if (!items.length) { event.preventDefault(); return; }
    const first = items[0], last = items[items.length - 1], at = document.activeElement;
    if (event.shiftKey && (at === first || at === sheet)) { event.preventDefault(); last.focus(); }
    else if (!event.shiftKey && at === last) { event.preventDefault(); first.focus(); }
  });
  // iOS keeps the layout viewport tall under the keyboard; follow the visible one.
  if (window.visualViewport) {
    const fit = () => document.documentElement.style.setProperty("--app-h", `${window.visualViewport.height}px`);
    window.visualViewport.addEventListener("resize", fit);
    window.visualViewport.addEventListener("scroll", () => window.scrollTo(0, 0));
    fit();
  }
  function renderView() {
    // Rebuilding the transcript can clamp the browser's scroll offset, so a
    // reader who is not pinned gets the same offset back afterwards.
    const keep = scroller().scrollTop;
    transcript.replaceChildren();
    transcript.className = "transcript";
    if (!state.view) {
      asks.replaceChildren(); asks.className = "asks"; renderedAsks = null;
      transcript.append(node("p", "Select a session to read its live view.", "status"));
      lastViewTabId = null; lastTailId = null; pinned = true; shownIds = new Set(); shownTab = null; updateJump();
      return;
    }
    if (state.view.tabId !== shownTab) { shownIds = new Set(); shownTab = state.view.tabId; }
    const seen = C.trackShown(shownIds, state.view.messages || [], state.view.asks || []);
    freshIds = seen.fresh; shownIds = seen.shown;
    for (const message of state.view.messages || []) {
      if (message.role === "thinking") continue;
      if (message.role === "tool") {
        transcript.append(renderTool(message));
      } else if (message.role === "assistant") {
        transcript.append(renderAssistant(message));
      } else if (message.role === "user") {
        transcript.append(renderUser(message));
      } else {
        transcript.append(renderNotice(message));
      }
    }
    renderAsks();
    afterRender(keep);
  }
  function renderAsks() {
    const nextAsks = JSON.stringify([state.view.tabId, state.view.asks || []]);
    if (nextAsks === renderedAsks) return;
    renderedAsks = nextAsks;
    asks.replaceChildren();
    asks.className = "asks";
    const liveDrafts = new Set();
    for (const ask of state.view.asks || []) {
      const key = `${state.view.tabId}:${ask.id}`;
      liveDrafts.add(key);
      asks.append(renderAsk(ask, key));
    }
    for (const key of askDrafts.keys()) if (!liveDrafts.has(key)) askDrafts.delete(key);
  }

  function railRow(tag, cls, glyphClass, symbol, content) {
    const row = node(tag, undefined, cls);
    const rail = node("div", undefined, "ass-rail");
    const glyph = node("div", undefined, glyphClass);
    if (symbol) glyph.append(node("span", symbol));
    rail.append(glyph, node("div", undefined, "ass-thread"));
    row.append(rail, content);
    return row;
  }

  function renderUser(message) {
    const row = node("div", undefined, `row user${fade(C.messageKey(message))}`);
    const bubble = node("div", undefined, "user-bubble");
    const text = node("div", message.text || "", "user-text selectable");
    bubble.append(text);
    row.append(bubble);
    return row;
  }

  function renderAssistant(message) {
    const body = node("div", undefined, "ass-body");
    const text = node("div", undefined, "ass-text selectable md-content");
    try { text.innerHTML = C.renderMarkdown(message.text || ""); }
    catch (err) { state = { ...state, error: err.message }; text.textContent = message.text || ""; }
    body.append(text);
    return railRow("div", `row assistant${fade(C.messageKey(message))}`, "ass-glyph", "✦", body);
  }

  function renderNotice(message) {
    const body = node("div", undefined, "ass-body");
    const text = node("div", message.text || "", "ass-text selectable");
    text.style.color = "var(--fg-3)";
    body.append(text);
    return railRow("div", `row assistant${fade(C.messageKey(message))}`, "ass-glyph", "", body);
  }

  function renderTool(message) {
    const tool = message.tool || {};
    const card = node("details", undefined, "tool-card");
    card.open = openTools.has(message.id);
    card.addEventListener("toggle", () => { if (card.open) openTools.add(message.id); else openTools.delete(message.id); });
    const head = node("summary", undefined, "tool-card-head");
    const tag = node("span", tool.name || "tool", "tool-tag");
    const title = node("span", tool.summary || "", "tool-title");
    const spacer = node("div", undefined, "tool-card-spacer");
    head.append(tag, title, spacer);
    const body = node("div", undefined, "tool-bash mono selectable");
    body.append(node("pre", tool.output || message.text || ""));
    card.append(head, body);
    return railRow("div", `row tool${fade(C.messageKey(message))}`, "tool-glyph", "⚙", card);
  }

  function renderAsk(ask, key) {
    const card = node("div", undefined, "ass-body");
    const row = railRow("article", `row ask${fade(`ask:${ask.id}`)}`, "ass-glyph ask-glyph", "?", card);
    card.append(node("div", ask.title || "Question", "ask-question"));
    if (ask.message) card.append(node("div", ask.message, "ask-confirm-message selectable"));
    if (ask.kind === "dialog") {
      card.append(dialogControls(ask, key));
      return row;
    }
    const controls = C.askControls(ask);
    if (Array.isArray(controls)) {
      const opts = node("div", undefined, "ask-options");
      for (const option of controls) {
        const b = button(option.label, () => action("answer", { askId: ask.id, value: option.value }));
        b.className = "ask-opt";
        opts.append(b);
      }
      card.append(opts);
    } else {
      const wrap = node("div", undefined, "ask-other");
      const input = node(controls.multiline ? "textarea" : "input");
      input.className = controls.multiline ? "ask-editor-textarea selectable" : "ask-other-input";
      input.setAttribute("aria-label", ask.title || "Answer");
      input.value = askDrafts.get(key) ?? ask.value ?? "";
      input.addEventListener("input", () => askDrafts.set(key, input.value));
      wrap.append(input);
      const submit = button(controls.multiline ? "Submit" : "Answer", () => action("answer", { askId: ask.id, value: input.value }));
      submit.className = "ask-submit";
      if (controls.multiline) {
        const cancel = button("Cancel", () => action("answer", { askId: ask.id, value: null }));
        cancel.className = "ask-opt";
        const actions = node("div", undefined, "ask-editor-actions");
        actions.append(submit, cancel);
        wrap.append(actions);
      } else {
        wrap.append(submit);
      }
      card.append(wrap);
    }
    return row;
  }

  // omp's multi-question ask: answers are [{ id, selectedOptions, customInput? }],
  // one per question in order. A pick-one question takes an option or typed
  // text, never both; a pick-any question may be answered with nothing.
  function dialogControls(ask, key) {
    const questions = ask.questions || [];
    const draft = askDrafts.get(key) ?? questions.map(() => ({ picks: [], text: "" }));
    askDrafts.set(key, draft);
    const wrap = node("div", undefined, "ask-dialog");
    const syncs = [];
    const problem = node("p", "", "error");
    questions.forEach((q, qi) => {
      const group = node("section", undefined, "ask-q");
      const head = node("div", undefined, "ask-q-head");
      if (questions.length > 1) head.append(node("span", `${qi + 1}/${questions.length}`, "mono"));
      if (q.header) head.append(node("span", q.header, "chip warn"));
      const kind = node("span", q.multi ? "pick any" : "pick one", `ask-q-kind${q.multi ? "" : " todo"}`);
      head.append(kind);
      group.append(head, node("div", q.question, "ask-question"));
      const rows = node("div", undefined, "ask-rows");
      const optionRows = [];
      (q.options || []).forEach((option, oi) => {
        const row = node("div", undefined, "ask-row");
        const btn = node("button");
        btn.type = "button";
        btn.className = "ask-row-btn";
        btn.setAttribute("role", q.multi ? "checkbox" : "radio");
        btn.addEventListener("click", () => {
          const entry = draft[qi];
          if (q.multi) {
            const picked = new Set(entry.picks);
            if (!picked.delete(option.label)) picked.add(option.label);
            entry.picks = q.options.map(o => o.label).filter(label => picked.has(label));
          } else { entry.picks = [option.label]; entry.text = ""; }
          sync();
        });
        const mark = node("span", "", `ask-mark ${q.multi ? "box" : "radio"}`);
        const text = node("span", option.label, "ask-row-text");
        if (option.description) text.append(node("span", option.description, "ask-row-desc"));
        btn.append(mark, text);
        row.append(btn);
        rows.append(row);
        optionRows.push(row);
      });
      const other = node("div", undefined, "ask-other");
      const input = node("input");
      input.className = "ask-other-input";
      input.setAttribute("aria-label", `Custom answer: ${q.question}`);
      input.placeholder = q.multi ? "Add your own…" : "Or type your own answer…";
      input.addEventListener("input", () => {
        draft[qi].text = input.value;
        if (!q.multi && input.value.trim()) draft[qi].picks = [];
        sync();
      });
      other.append(input);
      group.append(rows, other);
      wrap.append(group);
      function sync() {
        optionRows.forEach((row, i) => {
          const on = draft[qi].picks.includes(q.options[i].label);
          row.classList.toggle("selected", on);
          row.firstElementChild.setAttribute("aria-checked", String(on));
        });
        if (input.value !== draft[qi].text) input.value = draft[qi].text;
      }
      syncs.push(sync);
    });
    syncs.forEach(sync => sync());
    const answer = button("Answer", () => {
      const answers = [];
      for (const [qi, q] of questions.entries()) {
        const custom = draft[qi].text.trim();
        if (!q.multi && !draft[qi].picks.length && !custom) { problem.textContent = "Answer every question."; return; }
        answers.push(custom ? { id: q.id, selectedOptions: draft[qi].picks, customInput: custom } : { id: q.id, selectedOptions: draft[qi].picks });
      }
      problem.textContent = "";
      action("answer", { askId: ask.id, value: answers });
    });
    answer.className = "ask-submit";
    wrap.append(problem, answer);
    return wrap;
  }

  function connect() {
    connectionText = "Connecting…"; updateControls();
    socket = new WebSocket(`${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/ws`);
    socket.onopen = () => socket.send(JSON.stringify({ t: "auth", token }));
    socket.onmessage = event => {
      try {
        const msg = JSON.parse(event.data);
        state = C.reduce(state, msg);
        if (msg.t === "hello") {
          connected = true; attempt = 0; connectionText = "Connected";
          const wanted = pendingTab ?? selected;
          pendingTab = null;
          if (wanted) transmit(C.selectFor(wanted));
        }
        if (msg.t === "tabs") {
          if (state.view && !state.tabs.some(tab => tab.id === state.view.tabId)) {
            state = { ...state, view: null }; selected = null; renderView();
          }
          renderTabs();
        }
        if (msg.t === "view") { selected = state.view?.tabId ?? null; renderTabs(); renderView(); }
        if (msg.t === "theme") applyTheme(state.theme);
        updateControls();
      } catch (err) {
        state = { ...state, error: err.message }; updateControls();
      }
    };
    socket.onclose = event => {
      connected = false;
      state = { ...state, device: null, pending: {} };
      if (event.code === 4401) {
        token = null;
        localStorage.removeItem(TOKEN_KEY);
        connectionText = "This device is no longer paired — pair again from the desktop";
      } else {
        connectionText = event.code === 4503 ? "Desktop not ready — reconnecting…"
          : event.code === 4429 ? "Too many connections — reconnecting…" : "Connection lost — reconnecting…";
        setTimeout(connect, C.backoff(attempt++));
      }
      updateControls();
    };
    socket.onerror = () => { connectionText = "Connection failed — reconnecting…"; updateControls(); };
  }

  async function enableNotifications() {
    if (!connected || !state.pushKey) return;
    notifications.disabled = true;
    try {
      const permission = await Notification.requestPermission();
      if (permission !== "granted") throw new Error("Notification permission was not granted");
      const registration = await navigator.serviceWorker.register("/sw.js");
      // Wait for activation before pushManager.subscribe on a first visit.
      const ready = await navigator.serviceWorker.ready;
      const key = state.pushKey.replace(/-/g, "+").replace(/_/g, "/");
      const applicationServerKey = Uint8Array.from(atob(key), char => char.charCodeAt(0));
      const sub = await registration.pushManager.getSubscription()
        || await ready.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey });
      if (!transmit({ t: "push-subscribe", sub: sub.toJSON() })) throw new Error("Connection lost; enable notifications again after reconnecting");
      notificationsEnabled = true;
      notifications.textContent = "Notifications enabled";
    } catch (err) { state = { ...state, error: err.message }; }
    updateControls();
  }

  async function start() {
    try {
      const code = new URLSearchParams(location.hash.slice(1)).get("pair");
      if (code) {
        await pairFromCode(code);
        history.replaceState(null, "", location.pathname);
        return;
      }
      token = localStorage.getItem(TOKEN_KEY);
      if (token) connect();
      else { connectionText = "Pair this phone from the desktop"; updateControls(); }
    } catch (err) {
      connectionText = "Unable to connect";
      state = { ...state, error: err.message }; updateControls();
    }
  }
  renderView(); updateControls(); start();
})();
