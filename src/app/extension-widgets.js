// app/extension-widgets.js — omp extension status lines and text widgets in
// the rail (#52; docs/architecture/extension-widgets.md). Pure, no DOM:
//   apply(ext, frame)      one `extension_ui_request` frame → next state
//   fromSnapshot(snap)     the backend's `extension_ui_state` → state
//   isFireAndForget(m)     methods omp sends without waiting for a reply
//   cards(ext) / toggles(ext)
//                          WidgetCard models and Tweaks → Rail rows
// State is `{ statuses: [{key, text}], widgets: [{key, lines, more}] }`, keys
// in the order they first appeared: a replaced key keeps its place, so a card
// never jumps in the rail (omp's TUI sorts statuses and moves a replaced
// widget to the end). Key and line bounds match src-tauri/src/agent/ext_ui.rs;
// the backend keeps up to 4 KiB of raw text per line (escapes included) and
// this module strips, then shows 500 characters, so a snapshot and the live
// frames give the same cards for any line under that memory bound.
(function () {
  const LIMITS = { keys: 32, lines: 50, text: 500, keyBytes: 200 };
  const EMPTY = Object.freeze({ statuses: Object.freeze([]), widgets: Object.freeze([]) });
  // omp's `extension_ui_request` methods it never waits on: a reply to one is
  // noise (omp ignores responses for ids it is not waiting on).
  const FIRE_AND_FORGET = new Set(["notify", "setStatus", "setWidget", "setTitle", "set_editor_text"]);
  const STATUS_TWEAK = "widget:ext-status";
  const widgetTweak = (key) => `widget:ext:${key}`;

  // Terminal styling never reaches the rail: CSI (SGR colours, cursor moves),
  // OSC (OSC 8 hyperlinks keep their text, titles go), C1 CSI, any other
  // ESC sequence (charset designations too), then every remaining control
  // but tab and newline. A CSI may also end at the end of the text, where
  // the backend's byte cap can cut one off.
  const VT = /\u001b\[[0-?]*[ -/]*(?:[@-~]|$)|\u001b\][^\u0007\u001b]*(?:\u0007|\u001b\\)?|\u009b[0-?]*[ -/]*(?:[@-~]|$)|\u001b[ -/]*[0-~]?/g;
  const CONTROL = /[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g;
  function clean(text) {
    return window.truncateText(text.replace(VT, "").replace(CONTROL, ""), LIMITS.text);
  }

  // UTF-8 length without allocating: the backend bounds keys in bytes.
  function utf8Length(s) {
    let n = 0;
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      n += c < 0x80 ? 1 : c < 0x800 ? 2 : c >= 0xd800 && c <= 0xdbff ? (i++, 4) : 3;
    }
    return n;
  }
  const validKey = (k) => typeof k === "string" && k.length > 0 && utf8Length(k) <= LIMITS.keyBytes;

  // Replace `key`'s entry in place, append it while under the key bound, or
  // return `list` itself when it cannot be kept or is unchanged: extensions
  // resend the same status and widget often, and an unchanged state is no
  // `notify()` and no rail re-render.
  function upsert(list, entry, same) {
    const i = list.findIndex(e => e.key === entry.key);
    if (i !== -1) return same(list[i], entry) ? list : list.map((e, j) => (j === i ? entry : e));
    return list.length < LIMITS.keys ? [...list, entry] : list;
  }
  const sameStatus = (a, b) => a.text === b.text;
  const sameWidget = (a, b) => a.more === b.more && a.lines.length === b.lines.length && a.lines.every((l, i) => l === b.lines[i]);
  function remove(list, key) {
    return list.some(e => e.key === key) ? list.filter(e => e.key !== key) : list;
  }

  // `carried`: lines the backend already cut from a snapshot widget.
  function widgetEntry(key, rawLines, carried = 0) {
    // omp sends strings only; anything else is skipped, as in the backend.
    const strings = rawLines.filter(l => typeof l === "string");
    return { key, lines: strings.slice(0, LIMITS.lines).map(clean), more: Math.max(0, strings.length - LIMITS.lines) + carried };
  }

  function setStatus(ext, key, text) {
    if (!validKey(key)) return ext;
    // omp drops an `undefined` statusText from the JSON: absent = remove.
    const statuses = typeof text === "string"
      ? upsert(ext.statuses, { key, text: clean(text) }, sameStatus)
      : remove(ext.statuses, key);
    return statuses === ext.statuses ? ext : { ...ext, statuses };
  }

  function setWidget(ext, key, lines, carried) {
    if (!validKey(key)) return ext;
    const widgets = Array.isArray(lines)
      ? upsert(ext.widgets, widgetEntry(key, lines, carried), sameWidget)
      : remove(ext.widgets, key);
    return widgets === ext.widgets ? ext : { ...ext, widgets };
  }

  /** The state after one stdout frame; `ext` itself when nothing changed. */
  function apply(ext, frame) {
    if (!frame || frame.type !== "extension_ui_request") return ext;
    if (frame.method === "setStatus") return setStatus(ext, frame.statusKey, frame.statusText);
    if (frame.method === "setWidget") return setWidget(ext, frame.widgetKey, frame.widgetLines);
    return ext;
  }

  /** The backend's `ExtUiState` (`{statuses: [{key, text}], widgets: [{key,
   *  lines, more}]}`), normalised like live frames. Malformed input = empty. */
  function fromSnapshot(snap) {
    let ext = EMPTY;
    for (const s of Array.isArray(snap?.statuses) ? snap.statuses : []) ext = setStatus(ext, s?.key, s?.text);
    for (const w of Array.isArray(snap?.widgets) ? snap.widgets : []) {
      if (!Array.isArray(w?.lines)) continue;
      // The backend already cut the lines it did not keep; carry its count.
      ext = setWidget(ext, w.key, w.lines, Number.isInteger(w.more) && w.more > 0 ? w.more : 0);
    }
    return ext;
  }

  const isFireAndForget = (method) => FIRE_AND_FORGET.has(method);

  // A WidgetCard `run` for content omp pushed: always a finished, neutral
  // result with no timing.
  const pushed = (blocks) => ({
    state: "ok", widget: { status: "neutral", blocks, skipped: 0 },
    error: null, durationMs: null, at: null, lastOkAt: null,
  });

  /** Rail cards: one for all status lines (key/value rows), then one per
   *  widget key with its lines as monospace text. Each carries the Tweaks key
   *  that hides it. A widget with no lines has no card. */
  function cards(ext) {
    const out = [];
    if (ext.statuses.length > 0) {
      out.push({
        // The colon keeps both ids apart from plugin card ids ([a-z0-9-]).
        id: "ext-status:", title: "status", tweak: STATUS_TWEAK,
        run: pushed([{ type: "kv", rows: ext.statuses.map(s => ({ k: s.key, v: s.text })) }]),
      });
    }
    for (const w of ext.widgets) {
      if (w.lines.length === 0) continue;
      const more = w.more > 0 ? `\n… ${w.more} more line${w.more > 1 ? "s" : ""}` : "";
      out.push({
        id: `ext:${w.key}`, title: w.key, tweak: widgetTweak(w.key),
        run: pushed([{ type: "text", text: w.lines.join("\n") + more, mono: true }]),
      });
    }
    return out;
  }

  /** Tweaks → Rail rows for what the active tab has published. */
  function toggles(ext) {
    const rows = ext.statuses.length > 0 ? [{ tweak: STATUS_TWEAK, label: "extension: status lines" }] : [];
    for (const w of ext.widgets) rows.push({ tweak: widgetTweak(w.key), label: `extension: ${w.key}` });
    return rows;
  }

  window.OMP_EXT_WIDGETS = { EMPTY, apply, fromSnapshot, isFireAndForget, cards, toggles };
})();
