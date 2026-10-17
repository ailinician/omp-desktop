/* chat/chat-view.jsx — top-level chat surface. Sticks to the bottom while
   the user is already there; a manual scroll up unpins the view and shows
   a "Jump to latest" button instead of forcing the viewport back on every
   streamed update (issue #20). See app/scroll-pin.js for the pin state
   machine. Also routes each message to the right bubble component; the
   minimap-hover cross-highlight (mm-hot) flows through here via the
   `hoveredMsgIdx` prop. */

const { UserBubble: _CV_UserBubble, ToolCard: _CV_ToolCard, AssistantBubble: _CV_AssistantBubble, AskBubble: _CV_AskBubble, RetryRow: _CV_RetryRow, GoalRow: _CV_GoalRow, ApprovalRow: _CV_ApprovalRow, Icon: _CV_Icon, TOOL_META: _CV_TOOL_META } = window;
const { nextPinned: _CV_nextPinned, shouldRepin: _CV_shouldRepin } = window.OMP_SCROLL_PIN;
const { collapseStates: _CV_collapseStates, PREVIEW_LINES: _CV_PREVIEW_LINES } = window.OMP_COLLAPSE;
// Lines a collapsed block keeps in view, for chat.css; one object, so a
// streaming frame does not diff a fresh style.
const _CV_PAD_STYLE = { "--collapse-lines": _CV_PREVIEW_LINES };
var _t = (en) => (window.OMP_LANG === "en" ? en : (window.OMP_ZH_DICT?.[en] ?? en));


// ── Per-bubble memo wrappers ────────────────────────────────────────────────
// Primary streaming-perf win: only the live tail (new object ref per
// message_update) re-renders; stable messages bail out automatically.
// Created once at module load — React.memo returns a stable component type.
const _CV_UserBubble_M      = React.memo(_CV_UserBubble);
const _CV_ToolCard_M        = React.memo(_CV_ToolCard);
const _CV_AssistantBubble_M = React.memo(_CV_AssistantBubble);
const _CV_AskBubble_M       = React.memo(_CV_AskBubble);

const CompactRow = React.memo(function CompactRow({ msg }) {
  const [open, setOpen] = React.useState(false);
  const pending  = msg.status === "pending";
  const error    = msg.status === "error";
  const COLOR    = error ? "var(--rose)" : "var(--lilac)";
  // Shared with the status bar / usage-stats panel (adapter.js). Gate on
  // the *source* value being non-null, not the formatted string —
  // `formatTokens(null)` returns the truthy "—" placeholder, which would
  // render an empty "— before" chip instead of hiding it.
  const tok      = window.formatTokens(msg.tokensBefore);
  const hasBody  = !!msg.summary && !pending && !error;

  return (
    <div className="row tool fade-up">
      <div className="ass-rail">
        <div className="tool-glyph" style={{ borderColor: COLOR, color: COLOR }}>
          <_CV_Icon name={error ? "warn" : "context"} size={11} color={COLOR} />
        </div>
        <div className="ass-thread" />
      </div>
      <div className={`tool-card ${pending ? "running" : "ok"}`}
        style={error ? { borderColor: "color-mix(in oklab, var(--rose) 35%, var(--line-bright))" } : {}}>
        <div className="tool-card-head"
          style={{ cursor: hasBody ? "pointer" : "default" }}
          onClick={() => hasBody && setOpen(o => !o)}>
          <span className="tool-tag" style={{
            color: COLOR,
            background: `color-mix(in oklab, ${COLOR} 14%, transparent)`,
            borderColor: `color-mix(in oklab, ${COLOR} 30%, var(--line))`,
          }}>compact</span>
          <span className="tool-title">
            {pending ? _t("compacting context…")
              : error   ? _t("compaction failed")
              : (msg.shortSummary || _t("context compacted"))}
          </span>
          <div className="tool-card-spacer" />
          {pending && (
            <span className="chip accent" style={{ animation: "pulseDot 1.4s infinite" }}>
              <span className="dot live" />{" "}{_t("running")}
            </span>
          )}
          {!pending && !error && msg.tokensBefore != null && (
            <span className="chip muted mono">{tok} {_t("before")}</span>
          )}
          {error && <span className="chip" style={{ color: "var(--rose)" }}>{_t("failed")}</span>}
          {hasBody && <_CV_Icon name={open ? "chev" : "chevR"} size={10} color="var(--fg-4)" />}
        </div>
        {open && hasBody && (
          <div className="compact-body selectable">
            {msg.summary}
          </div>
        )}
      </div>
    </div>
  );
});

// A background job whose result omp just delivered (`async-result`,
// app/turn-status.js `finishedJobsOf`): the agent's next reply is what that
// result woke it for, so the job is named above it. Live-only, like tool
// cards — get_messages' merge keeps it in place.
const JobRow = React.memo(function JobRow({ msg, idx, highlighted }) {
  const { color, icon, label } = _CV_TOOL_META.job;
  return (
    <div className={`row tool fade-up${highlighted ? " mm-hot" : ""}`} data-msg-idx={idx}>
      <div className="ass-rail">
        <div className="tool-glyph" style={{ borderColor: color, color }}>
          <_CV_Icon name={icon} size={11} color={color} />
        </div>
        <div className="ass-thread" />
      </div>
      <div className="tool-card ok">
        <div className="tool-card-head">
          <span className="tool-tag" style={{
            color,
            background: `color-mix(in oklab, ${color} 14%, transparent)`,
            borderColor: `color-mix(in oklab, ${color} 30%, var(--line))`,
          }}>{label}</span>
          <span className="tool-title" title={msg.label ?? undefined}>
            {msg.jobId} {_t("finished")}
            {msg.label && <span className="job-label"> · {msg.label}</span>}
          </span>
          <div className="tool-card-spacer" />
          {msg.type && <span className="chip muted mono">{msg.type}</span>}
          {msg.durationMs != null && <span className="chip muted mono">{window.OMP_SUBAGENTS.fmtDuration(msg.durationMs)}</span>}
        </div>
      </div>
    </div>
  );
});

// Collapsible blocks (#44, #43): `foldModes` (`{ thinking, tool }`) and
// `collapseAfter` are the tweaks, `collapseOverrides` this tab's
// `_id → collapsed` toggles, and `onToggleCollapse(id, collapsed)` flips one
// (app/chat-collapse.js).
// ── Pure Turn-Rail helpers (extracted for isolated testability) ─────────────
const TURN_POP_PAD_Y = 88;

function extractUserTurns(messages, isHidden, tr = _t) {
  const out = [];
  for (let i = 0; i < messages.length; i++) {
    const m = messages[i];
    if (!m || m.kind !== "user" || isHidden(m)) continue;
    const raw = (m.text || "").replace(/\s+/g, " ").trim();
    const text = raw || (m.images?.length ? `[${tr("image")} ×${m.images.length}]` : tr("(empty)"));
    out.push({ turnNum: out.length + 1, msgIdx: i, id: m._id ?? i, text, time: m.time || "" });
  }
  return out;
}

function clampPopY(centerY, wrapHeight) {
  return Math.max(TURN_POP_PAD_Y, Math.min(wrapHeight - TURN_POP_PAD_Y, centerY));
}

function stepTurn(hoverTurn, total, deltaY) {
  if (hoverTurn === null || total < 2 || deltaY === 0) return hoverTurn;
  const dir = deltaY > 0 ? 1 : -1;
  return Math.max(0, Math.min(total - 1, hoverTurn + dir));
}

function fisheyeWindow(userTurns, hoverTurn) {
  if (hoverTurn === null || !userTurns[hoverTurn]) return [];
  const slice = [];
  for (let offset = -1; offset <= 1; offset++) {
    const idx = hoverTurn + offset;
    const item = userTurns[idx];
    if (!item) continue;
    const posClass = offset === 0 ? "is-focus" : (offset < 0 ? "is-neighbor is-prev" : "is-neighbor is-next");
    slice.push({ idx, offset, item, posClass });
  }
  return slice;
}

function ChatView({ messages, planMode, annotations, onAnnotate, hoveredMsgIdx, onAskAnswer, onAskDialogAnswer, onConfirmAsk, onCancelAsk, onGrantApproval, onStopRetry, hasProjectPath, onInspectSubagent, onOpenFile, foldModes, collapseAfter, collapseOverrides, onToggleCollapse }) {
  const scrollRef   = React.useRef(null);
  const pinnedRef   = React.useRef(true);   // assume start pinned to bottom
  const prevTopRef  = React.useRef(0);
  const lastIdRef   = React.useRef(null);
  // Mirrors pinnedRef into render so the "Jump to latest" button can show —
  // scroll position itself must not force a re-render on every wheel tick.
  const [showJump, setShowJump] = React.useState(false);
  // While the user reads history: the last message shown when they left the
  // bottom. Newer blocks don't auto-collapse older ones meanwhile, so the
  // text being read doesn't fold away under the reader.
  const [collapseHorizon, setCollapseHorizon] = React.useState(null);

  const [activeTurn, setActiveTurn] = React.useState(0);
  const activeTurnRef = React.useRef(0);
  const [hoverTurn, setHoverTurn]   = React.useState(null);
  const [popY, setPopY]             = React.useState(120);
  const [flashIdx, setFlashIdx]     = React.useState(null);
  const hideTimerRef                = React.useRef(null);
  const flashTimerRef               = React.useRef(null);
  const wrapRef                     = React.useRef(null);

  const userTurns = React.useMemo(
    () => extractUserTurns(messages, window.OMP_TURN_STATUS.isHidden, _t),
    [messages],
  );

  const syncActiveTurn = React.useCallback((el) => {
    if (!el || !userTurns.length) return;
    if (pinnedRef.current) {
      const last = userTurns.length - 1;
      if (activeTurnRef.current !== last) {
        activeTurnRef.current = last;
        setActiveTurn(last);
      }
      return;
    }
    const threshold = el.scrollTop + el.clientHeight * 0.42;
    let found = 0;
    for (let k = 0; k < userTurns.length; k++) {
      const node = el.querySelector(`[data-msg-idx="${userTurns[k].msgIdx}"]`);
      if (node && node.offsetTop <= threshold) found = k;
      else if (node && node.offsetTop > threshold) break;
    }
    if (activeTurnRef.current !== found) {
      activeTurnRef.current = found;
      setActiveTurn(found);
    }
  }, [userTurns]);
  const setPinned = (val) => {
    pinnedRef.current = val;
    setShowJump(!val);
    setCollapseHorizon(val ? null : lastIdRef.current);
  };

  // Re-syncs prevTopRef after every programmatic scroll, or the next
  // nextPinned call would misread it as a user scroll-up.
  const stickToBottom = (el) => {
    el.scrollTop = el.scrollHeight;
    prevTopRef.current = el.scrollTop;
  };

  // Only a real user scroll — up and away from the bottom — unpins.
  // Scrolling back down near the bottom re-pins. See app/scroll-pin.js.
  const onScroll = () => {
    const el = scrollRef.current;
    if (!el) return;
    const pinned = _CV_nextPinned(pinnedRef.current, prevTopRef.current, el);
    prevTopRef.current = el.scrollTop;
    if (pinned !== pinnedRef.current) setPinned(pinned);
    syncActiveTurn(el);
  };

  const jumpToLatest = () => {
    const el = scrollRef.current;
    if (!el) return;
    stickToBottom(el);
    setPinned(true);
  };

  // Auto-scroll only while pinned. A new message block (tool card, new
  // assistant turn, …) during a run must NOT itself force a jump — that
  // was issue #20: every streamed update snapped the view to the bottom
  // even while the user was reading history. Sending a new prompt
  // explicitly re-pins, and so does the transcript being reset to empty
  // (/new, a profile switch respawning the tab, or the tab itself being
  // torn down) — none of those change `activeSessionId`, so the `key`
  // remount on tab switch (app-live.jsx) doesn't reach them, and a leftover
  // unpinned state would otherwise strand the button over an empty chat.
  React.useEffect(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (!pinnedRef.current && _CV_shouldRepin(lastIdRef.current, messages)) setPinned(true);
    lastIdRef.current = messages[messages.length - 1]?._id ?? null;
    if (pinnedRef.current) stickToBottom(el);
    syncActiveTurn(el);
  }, [messages, syncActiveTurn]);

  // The composer below grows and shrinks on its own (the queue strip, image
  // attachments, a growing draft), which shortens this view from below
  // with no scroll event and no new message: keep a pinned view on the
  // latest line through that. `stickToBottom` reads refs only, so the
  // first render's copy stays valid.
  React.useEffect(() => {
    const el = scrollRef.current;
    if (!el) return undefined;
    const ro = new ResizeObserver(() => { if (pinnedRef.current) stickToBottom(el); });
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  React.useEffect(() => () => {
    clearTimeout(hideTimerRef.current);
    clearTimeout(flashTimerRef.current);
  }, []);

  // Only the last completed assistant message is annotatable in plan mode
  let lastAsstIdx = -1;
  if (planMode) {
    for (let i = messages.length - 1; i >= 0; i--) {
      if (messages[i].kind === "assistant" && !messages[i].streaming && !messages[i].superseded) { lastAsstIdx = i; break; }
    }
  }

  const collapsed = React.useMemo(() => _CV_collapseStates(messages, {
    ...foldModes, after: collapseAfter, overrides: collapseOverrides,
    horizonId: collapseHorizon, hidden: window.OMP_TURN_STATUS.isHidden,
  }), [messages, foldModes.thinking, foldModes.tool, collapseAfter, collapseOverrides, collapseHorizon]);

  const clearHideTimer = () => {
    clearTimeout(hideTimerRef.current);
    hideTimerRef.current = null;
  };

  const scheduleHideRail = () => {
    clearHideTimer();
    hideTimerRef.current = setTimeout(() => setHoverTurn(null), 110);
  };

  const updatePopYForTurn = (k, dotEl) => {
    const wrap = wrapRef.current;
    if (!wrap) return;
    const wrapRect = wrap.getBoundingClientRect();
    const targetEl = dotEl || wrap.querySelector(`[data-turn-dot="${k}"]`);
    if (!targetEl) return;
    const dotRect = targetEl.getBoundingClientRect();
    const centerY = dotRect.top - wrapRect.top + dotRect.height / 2;
    setPopY(clampPopY(centerY, wrapRect.height));
  };

  const onEnterDot = (k, e) => {
    clearHideTimer();
    setHoverTurn(k);
    updatePopYForTurn(k, e.currentTarget);
  };

  const onRailWheel = (e) => {
    const next = stepTurn(hoverTurn, userTurns.length, e.deltaY);
    if (next === hoverTurn || next === null) return;
    e.preventDefault();
    setHoverTurn(next);
    updatePopYForTurn(next, null);
  };

  const jumpToTurn = (k) => {
    const t = userTurns[k];
    const el = scrollRef.current;
    if (!t || !el) return;
    const rowEl = el.querySelector(`[data-msg-idx="${t.msgIdx}"]`);
    if (!rowEl) return;
    const isLast = k === userTurns.length - 1;
    if (!isLast) setPinned(false);
    const targetTop = Math.max(0, rowEl.offsetTop - 24);
    prevTopRef.current = targetTop;
    el.scrollTo({ top: targetTop, behavior: "smooth" });
    activeTurnRef.current = k;
    setActiveTurn(k);
    setFlashIdx(t.msgIdx);
    clearTimeout(flashTimerRef.current);
    flashTimerRef.current = setTimeout(() => setFlashIdx(null), 1500);
  };

  const hoveredTurnMsgIdx = hoverTurn !== null ? userTurns[hoverTurn]?.msgIdx : null;
  const fisheyeCards = fisheyeWindow(userTurns, hoverTurn);
  return (
    <div className="chat-wrap" ref={wrapRef}>
      <div className="chat-scroll selectable" ref={scrollRef} onScroll={onScroll}>
        <div className="chat-pad" style={_CV_PAD_STYLE}>
          {/* Note: after a trim, idx shifts for all surviving messages, causing a full
             re-render of memo'd bubbles in that notify cycle (bounded at MINIMAP_MAX -
             MINIMAP_COLS = 156). Streaming re-renders are unaffected. Driving annotable
             and scroll-targeting off _id instead of idx would make trim zero-cost for
             history, but requires a larger refactor. */}
          {messages.map((m, i) => {
            const hl = hoveredMsgIdx === i || hoveredTurnMsgIdx === i || flashIdx === i;
            if (window.OMP_TURN_STATUS.isHidden(m)) return null;
            if (m.kind === "user")    return <_CV_UserBubble_M    key={m._id ?? i} idx={i} highlighted={hl} msg={m} />;
            if (m.kind === "compact") return <CompactRow          key={m._id ?? i} msg={m} />;
            if (m.kind === "job")     return <JobRow              key={m._id ?? i} idx={i} highlighted={hl} msg={m} />;
            if (m.kind === "retry")   return <_CV_RetryRow        key={m._id ?? i} idx={i} highlighted={hl} msg={m} onStop={onStopRetry} />;
            if (m.kind === "goal")    return <_CV_GoalRow         key={m._id ?? i} idx={i} highlighted={hl} msg={m} />;
            if (m.kind === "approval") return <_CV_ApprovalRow    key={m._id ?? i} idx={i} highlighted={hl} msg={m} />;
            if (m.kind === "tool")    return <_CV_ToolCard_M      key={m._id ?? i} idx={i} highlighted={hl} msg={m} onInspectSubagent={onInspectSubagent}
              onOpenFile={onOpenFile} collapsed={collapsed[m._id]} onToggleCollapse={onToggleCollapse} />;
            if (m.kind === "ask")     return <_CV_AskBubble_M     key={m._id ?? i} idx={i} highlighted={hl} msg={m} onAnswer={onAskAnswer} onAnswerDialog={onAskDialogAnswer} onConfirm={onConfirmAsk} onCancelAsk={onCancelAsk} onGrant={onGrantApproval} hasProjectPath={hasProjectPath} />;
            return <_CV_AssistantBubble_M key={m._id ?? i} idx={i} highlighted={hl} msg={m}
              annotable={i === lastAsstIdx}
              annotations={annotations}
              onAnnotate={onAnnotate}
              thoughtCollapsed={collapsed[m._id]} onToggleCollapse={onToggleCollapse} />;
          })}
          <div style={{ height: 24 }} />
        </div>
      </div>
      {userTurns.length > 0 && (
        <div
          className={`turn-rail${hoverTurn !== null ? " is-open" : ""}`}
          onMouseEnter={clearHideTimer}
          onMouseLeave={scheduleHideRail}
          onWheel={onRailWheel}
        >
          <div className="turn-rail-track">
            {userTurns.map((t, k) => {
              const isCurrent = k === activeTurn;
              const isHot     = k === hoverTurn;
              const isAdj     = hoverTurn !== null && Math.abs(k - hoverTurn) === 1;
              return (
                <button
                  key={t.id}
                  type="button"
                  data-turn-dot={k}
                  className={`turn-dot-btn${isCurrent ? " is-current" : ""}${isHot ? " is-hot" : ""}${isAdj ? " is-adj" : ""}`}
                  onMouseEnter={(e) => onEnterDot(k, e)}
                  onClick={() => jumpToTurn(k)}
                  aria-label={`${_t("Jump to prompt #")}${t.turnNum}`}
                >
                  <span className="turn-dot" />
                </button>
              );
            })}
          </div>
          {fisheyeCards.length > 0 && (
            <div className="turn-pop" style={{ top: `${popY}px` }}>
              {fisheyeCards.map(({ idx, offset, item, posClass }) => (
                <div
                  key={item.id}
                  className={`turn-card ${posClass}`}
                  onClick={() => jumpToTurn(idx)}
                >
                  <div className="turn-card-hd">
                    <span className="turn-card-badge mono">
                      #{item.turnNum}{offset === 0 ? ` / ${userTurns.length}` : ""}
                    </span>
                    {item.time && <span className="turn-card-time mono">{item.time}</span>}
                    {offset === 0 && <span className="turn-card-hint">{_t("click to jump")}</span>}
                  </div>
                  <div className="turn-card-text">{item.text}</div>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
      {showJump && (
        <button type="button" className="chat-jump-latest" onClick={jumpToLatest}>
          <_CV_Icon name="chev" size={12} />
          {_t("Jump to latest")}
        </button>
      )}
    </div>
  );
}

Object.assign(window, { ChatView, OMP_TURN_RAIL: Object.freeze({ extractUserTurns, clampPopY, stepTurn, fisheyeWindow }) });
