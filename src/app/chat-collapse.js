// Collapsible tool cards and thinking blocks in the chat (#44, #43).
//
// Thinking and tool output each have a mode (Settings → Chat):
// - "auto": every block starts expanded; once `after` newer collapsible
//   blocks have followed it, a long block collapses on its own to a few
//   lines of preview; a short one never does.
// - "open": never collapses on its own.
// - "folded": collapsed from the start, except a tool card the reader needs
//   to see: a running one, an edit's diff, a failed call.
// The user's own toggle on a block always wins and is kept per tab, keyed by
// the message's `_id` (live.js assigns it, and the get_messages merge
// carries it over, app/transcript-merge.js).
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

  /** The "collapse after" tweak: newer blocks a block waits for in "auto". */
  const DEFAULT_AFTER = 5;
  const MIN_AFTER = 1;
  const MAX_AFTER = 30;

  /** Per-kind modes, in the order Settings lists them. */
  const MODES = Object.freeze(["auto", "open", "folded"]);

  /** A tab with no toggles yet. Shared, so a render without one compares equal. */
  const NO_OVERRIDES = Object.freeze({});

  /** `{ thinking, tool }` modes from the tweaks (`foldThinking`,
   *  `foldTools`). A mode not set yet comes from the switch it replaced:
   *  `autoCollapse: false` (auto-collapse off) means "open". */
  function foldModes(tweaks) {
    const legacy = tweaks?.autoCollapse === false ? "open" : "auto";
    const mode = v => (MODES.includes(v) ? v : legacy);
    return { thinking: mode(tweaks?.foldThinking), tool: mode(tweaks?.foldTools) };
  }

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

  // A tool card "folded" still shows: running output, an edit's diff (what
  // changed is the point of the card), a failed call (omp's `isError`).
  const keepsOpen = m => m.kind === "tool" && (isLive(m) || m.tool === "edit" || !!m.isError);

  // Whether a block with a toggle starts collapsed under `mode`; `aged`:
  // enough newer blocks follow it.
  function _collapsedBy(mode, m, lines, aged) {
    if (mode === "open") return false;
    if (mode === "folded") return !keepsOpen(m);
    return aged && lines > SHORT_LINES && !isLive(m);
  }

  /** `_id → collapsed` for every block in `messages` that has a toggle.
   *  - `thinking` / `tool`: the two kinds' modes (`foldModes`).
   *  - `after`: newer collapsible blocks an old one waits for in "auto"
   *    (the tweak, clamped by `clampAfter`). Blocks of both kinds count,
   *    whatever their own mode.
   *  - `overrides`: `_id → collapsed` the user toggled; always wins.
   *  - `horizonId`: while the user reads history, the `_id` of the last
   *    message shown when they scrolled away; blocks after it do not age
   *    older ones, so nothing collapses under the reader. Null when the
   *    view follows the latest message, or when that message is gone.
   *  - `hidden(msg)`: messages the chat does not render. */
  function collapseStates(messages, { thinking = "auto", tool = "auto", after, overrides = NO_OVERRIDES, horizonId = null, hidden = () => false } = {}) {
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
        : _collapsedBy(m.kind === "tool" ? tool : thinking, m, lines, newer >= wait);
      if (i <= horizon) newer++;
    }
    return out;
  }

  /** Toggles that set every block with a toggle in `messages` to
   *  `collapsed` (Collapse all / Expand all). A live block (running tool,
   *  streaming reply) gets one too, even before it is long enough to have a
   *  toggle: it is on screen when the command runs, so it must not fall back
   *  to the modes once it grows. A block that arrives later has none, so it
   *  follows the modes again. */
  function overridesForAll(messages, collapsed, { hidden = () => false } = {}) {
    const out = {};
    for (const m of messages) {
      if (m._id == null || hidden(m)) continue;
      if (!isLive(m) && measuredLines(m) <= PREVIEW_LINES) continue;
      out[m._id] = collapsed;
    }
    return out;
  }

  window.OMP_COLLAPSE = {
    PREVIEW_LINES, SHORT_LINES, DEFAULT_AFTER, MIN_AFTER, MAX_AFTER, MODES, NO_OVERRIDES,
    foldModes, clampAfter, bodyLines, bashPreview, collapseStates, overridesForAll,
  };
})();
