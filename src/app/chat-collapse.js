// Collapsible tool cards and thinking blocks in the chat (#44).
//
// Every block starts expanded. Once `after` newer collapsible blocks have
// followed it, a long block collapses on its own to a few lines of preview;
// a short one never does. The user's own toggle on a block always wins and
// is kept per tab, keyed by the message's `_id` (live.js assigns it, and the
// get_messages merge carries it over, app/transcript-merge.js).
//
// Exposes `window.OMP_COLLAPSE`; wrapped as an IIFE per the project rule for
// plain <script> tags (AGENTS.md "IIFE rule"). Pure and dependency-free
// (test-chat-collapse.mjs).
(function () {
  /** Lines a collapsed block keeps in view. A block no longer than this
   *  has nothing to hide, so it gets no toggle and does not count. */
  const PREVIEW_LINES = 3;
  /** A block no longer than this never collapses on its own. */
  const SHORT_LINES = 8;
  /** Characters per wrapped line of prose, for thinking text. */
  const WRAP_CHARS = 100;

  /** The "auto-collapse after" tweak: newer blocks a block waits for. */
  const DEFAULT_AFTER = 5;
  const MIN_AFTER = 1;
  const MAX_AFTER = 30;

  /** A tab with no toggles yet. Shared, so a render without one compares equal. */
  const NO_OVERRIDES = Object.freeze({});

  /** Coerces the tweak into [MIN_AFTER, MAX_AFTER]; anything non-numeric
   *  falls back to DEFAULT_AFTER. */
  function clampAfter(n) {
    const v = Math.floor(Number(n));
    if (!Number.isFinite(v)) return DEFAULT_AFTER;
    return Math.min(MAX_AFTER, Math.max(MIN_AFTER, v));
  }

  const wrapped = text => Math.max(1, Math.ceil(text.length / WRAP_CHARS));
  // Preformatted text (eval code and output, bash lines) keeps its line
  // breaks and wraps long lines (`pre-wrap`).
  const preLines = text => {
    let n = 0;
    for (const line of text.trimEnd().split("\n")) n += wrapped(line);
    return n;
  };

  /** How many lines the collapsible part of `msg` renders: a tool card's
   *  body (chat/tool-card.jsx) or an assistant reply's thinking. 0 when it
   *  has none. Estimated from the data, so it is known before layout. */
  function bodyLines(msg) {
    if (msg.kind === "assistant") return msg.thought ? wrapped(msg.thought) : 0;
    if (msg.kind !== "tool") return 0;
    switch (msg.tool) {
      case "bash":   return (msg.output ?? []).reduce((n, l) => n + wrapped(l.line), 0);
      // Diff header and scrub track around the diff lines; else one foot line.
      case "edit":   return msg.diff ? msg.diff.length + 2 : 1;
      case "read":   return 1;
      case "search": return msg.preview?.length ?? 0;
      case "task":   return msg.subagents?.length ?? 0;
      case "eval":
        return (msg.cells ?? []).reduce((n, c) =>
          n + 1 + (c.code ? preLines(c.code) : 0) + (c.output ? preLines(c.output) : 0), 0);
      default:       return 0;
    }
  }

  // Still receiving output: collapsing it would hide what is happening now.
  const isLive = msg => (msg.kind === "tool" ? msg.status === "running" : !!msg.streaming);

  // `bodyLines` per message object. The chat recomputes the states on every
  // streaming frame, but only the streaming entry is a new object then, so
  // settled ones (eval output can be long) are not measured again. A live
  // entry is always measured afresh: live.js grows `streamingBubble` in place
  // (message_update), and that very object sits in the transcript after
  // message_start and after a get_messages merge. A settled entry is never
  // changed in place — every update replaces it with a new object.
  const _measured = new WeakMap();
  function measuredLines(msg) {
    if (isLive(msg)) return bodyLines(msg);
    let n = _measured.get(msg);
    if (n === undefined) { n = bodyLines(msg); _measured.set(msg, n); }
    return n;
  }

  /** A collapsed bash card's lines: the last PREVIEW_LINES that are not
   *  blank. omp ends the output with blank lines and its notices (exit code,
   *  wall time), so a plain tail could show nothing but empty lines. */
  function bashPreview(output) {
    const out = [];
    for (let i = output.length - 1; i >= 0 && out.length < PREVIEW_LINES; i--) {
      if (output[i].line.trim()) out.unshift(output[i]);
    }
    return out;
  }

  /** `_id → collapsed` for every block in `messages` that has a toggle.
   *  - `auto`: collapse old long blocks on their own (the tweak's switch).
   *  - `after`: newer collapsible blocks an old one waits for (the tweak,
   *    clamped by `clampAfter`).
   *  - `overrides`: `_id → collapsed` the user toggled; always wins.
   *  - `horizonId`: while the user reads history, the `_id` of the last
   *    message shown when they scrolled away; blocks after it do not age
   *    older ones, so nothing collapses under the reader. Null when the
   *    view follows the latest message, or when that message is gone.
   *  - `hidden(msg)`: messages the chat does not render. */
  function collapseStates(messages, { auto = true, after, overrides = NO_OVERRIDES, horizonId = null, hidden = () => false } = {}) {
    const wait = clampAfter(after);
    let horizon = messages.length - 1;
    if (horizonId != null) {
      const at = messages.findIndex(m => m._id === horizonId);
      if (at !== -1) horizon = at;
    }
    const out = {};
    let newer = 0;
    for (let i = messages.length - 1; i >= 0; i--) {
      const m = messages[i];
      if (m._id == null || hidden(m)) continue;
      const lines = measuredLines(m);
      if (lines <= PREVIEW_LINES) continue;
      out[m._id] = Object.hasOwn(overrides, m._id)
        ? overrides[m._id]
        : auto && newer >= wait && lines > SHORT_LINES && !isLive(m);
      if (i <= horizon) newer++;
    }
    return out;
  }

  window.OMP_COLLAPSE = {
    PREVIEW_LINES, SHORT_LINES, DEFAULT_AFTER, MIN_AFTER, MAX_AFTER, NO_OVERRIDES,
    clampAfter, bodyLines, bashPreview, collapseStates,
  };
})();
