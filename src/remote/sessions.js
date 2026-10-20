// Phone fixed header and modal session sheet.
// Exposes window.OMP_REMOTE_SESSIONS = { createSessions({ onSelectTab, onEnableNotifications }) }.
(function () {
  const C = window.OMP_REMOTE_CLIENT;

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

  // Dot like the desktop tab's run dot: accent pulse while running, amber
  // "?" while a question waits, nothing visible when idle.
  function setDot(dot, tab) {
    const kind = tab?.hasAsk ? "waiting-user question" : tab?.busy ? "running" : "idle";
    dot.className = `tab-run-dot ${kind}`;
    dot.textContent = tab?.hasAsk ? "?" : "";
    dot.title = tab?.hasAsk ? "waiting for you" : tab?.busy ? "running" : "";
  }

  function createSessions({ onSelectTab, onEnableNotifications }) {
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
    const notifications = button("Enable notifications", onEnableNotifications);
    const sheetConn = node("span", "", "sheet-conn");
    sheetConn.setAttribute("role", "status");
    const sheetFoot = node("footer", undefined, "sheet-foot");
    const sheetInfo = node("div", undefined, "sheet-info");
    sheetInfo.append(node("strong", "OMP Remote"), sheetConn);
    sheetFoot.append(sheetInfo, notifications);
    sheet.append(handle, sheetList, sheetFoot);

    let sheetOpen = false, sheetSignature = null;

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

    function render(tabs, activeId) {
      const active = tabs.find(tab => tab.id === activeId);
      const title = active ? active.title || "Untitled" : tabs.length ? "Select a session" : "OMP Remote";
      const project = C.projectName(active?.project);
      const others = C.otherQuestionCount(tabs, activeId);
      topTitle.textContent = title;
      topProject.textContent = project;
      topProject.hidden = !project;
      topCount.textContent = String(others);
      topCount.hidden = others === 0;
      setDot(topDot, active);
      header.setAttribute("aria-label", `${title}${project ? `, ${project}` : ""}${others ? `, ${others} other ${others === 1 ? "session has a question" : "sessions have questions"}` : ""}. Switch session`);
      // The sheet list is rebuilt only when it changed, so focus survives updates.
      const signature = JSON.stringify([activeId, tabs]);
      if (signature === sheetSignature) return;
      sheetSignature = signature;
      const focusedTabId = sheetOpen ? (document.activeElement?.getAttribute?.("data-tab-id") ?? document.activeElement?.["data-tab-id"] ?? null) : null;
      sheetList.replaceChildren();
      if (!tabs.length) sheetList.append(node("p", "No sessions open on the desktop.", "status"));
      let restoreFocus = null;
      for (const tab of tabs) {
        const item = button("", () => onSelectTab(tab.id, closeSheet));
        item.className = `sheet-tab${tab.id === activeId ? " active" : ""}`;
        item.setAttribute("data-tab-id", tab.id);
        if (tab.id === focusedTabId) restoreFocus = item;
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
      if (focusedTabId) (restoreFocus ?? sheet).focus();
    }

    function syncControls({ connected, connectionText, canPush, pushKey, notificationsEnabled, notificationsPending }) {
      sheetConn.textContent = connectionText;
      connDot.className = `conn-dot ${connected ? "ok" : "off"}`;
      connDot.title = connected ? "Connected" : "Reconnecting";
      notifications.textContent = notificationsEnabled ? "Notifications enabled" : "Enable notifications";
      notifications.hidden = !canPush;
      notifications.disabled = !connected || !pushKey || notificationsEnabled || notificationsPending;
    }

    return { header, backdrop, sheet, render, syncControls };
  }

  window.OMP_REMOTE_SESSIONS = { createSessions };
})();
