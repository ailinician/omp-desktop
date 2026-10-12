# Collapsible tool cards and thinking (#44)

Code: `src/app/chat-collapse.js` (rules, `window.OMP_COLLAPSE`), `src/design/ui/collapse-toggle.jsx` (control), `chat/tool-card.jsx` / `chat/assistant-bubble.jsx` (rendering), `chat/chat-view.jsx` (wiring), `app-live.jsx` (per-tab toggles, Tweaks → Chat). Test: `tests/test-chat-collapse.mjs`.

## Blocks

A *block* is a tool card's body or an assistant reply's thinking. `bodyLines(msg)` estimates its rendered lines from the data (bash and eval lines wrapped at 100 chars as `pre-wrap` does, diff lines + header/track, search hits, subagent rows, thinking as prose wrapped at 100 chars), so the decision is known before layout.

`collapseStates` runs on every streaming frame, so the estimate is cached per message object (`WeakMap`): a settled entry is never changed in place — live.js replaces it with a new object on every update — so it is measured once. A live entry (running tool, streaming reply) is measured afresh every time, because live.js grows `streamingBubble` in place and that object itself sits in the transcript after `message_start` and after a get_messages merge (`live.js` `[...merged, streamingBubble]`). A new in-place write to a settled transcript entry would leave its cached size stale.

| Lines | Toggle | Auto-collapse | Ages older blocks |
|---|---|---|---|
| ≤ `PREVIEW_LINES` (3) | no — nothing to hide | no | no |
| ≤ `SHORT_LINES` (8) | yes | never | yes |
| longer | yes | yes | yes |

## Rule

`collapseStates(messages, { auto, after, overrides, horizonId, hidden })` → `_id → collapsed` for every block with a toggle.

- Everything starts expanded. A long block collapses once `after` newer blocks (tweak *collapse after*, 1–30, default 5) follow it.
- A running tool card or a streaming reply never collapses on its own.
- The user's toggle (`overrides`) always wins, both ways, including on short blocks.
- `auto: false` (tweak *auto-collapse* off): only user toggles collapse.
- Messages the chat hides (`OMP_TURN_STATUS.isHidden`) neither collapse nor count.

## Not collapsing under the reader

While the view is unpinned (the user scrolled up, `app/scroll-pin.js`), ChatView passes `horizonId` = the last message's `_id` when they left the bottom: blocks after it do not age older ones, so the text being read does not fold away. Re-pinning clears it and the backlog collapses while the view sticks to the bottom. A horizon no longer in the transcript freezes nothing.

## Toggles per tab

`app-live.jsx` keeps `sessionId → { _id → collapsed }` (`OMP_SESSION_UI.entryOf`/`updateEntry`/`pruneEntries`, like plan mode), so a tab switch — which remounts ChatView — keeps them. `_id` is live.js's per-message counter; `mergeTranscript` copies the live entry's `_id` onto its get_messages copy (`_withLiveFields`), or the merge after a tab switch would re-key the bubble and drop its toggle. Closing a tab drops its toggles; `_trimMessages` leaves stale ids behind, which are harmless.

## Rendering

Collapsed, a block keeps `--collapse-lines` (= `PREVIEW_LINES`, set on `.chat-pad`) in view behind a fade (`.collapse-clip`, `-webkit-mask-image` for WebKit). bash shows its *last* non-blank lines (`bashPreview`: omp ends the output with blank lines and its exit-code/wall-time notices, so a plain tail could be empty); its clip (`.from-end`) bottom-aligns them, so when they wrap the overflow is cut at the top and the final line stays in view, and the floating copy button (which still copies the whole output) sits at the bottom, out of the fade. Everything else shows its first lines. Thinking uses a line clamp. Each block's props are a boolean and a stable callback, so the `React.memo` wrappers in `chat-view.jsx` still skip unchanged bubbles.

Clipped content stays in the DOM, so controls below the preview (later eval cells' copy buttons, subagent rows) remain focusable. Not made `inert` on purpose: that would also block selecting the preview text and the bash copy button.
