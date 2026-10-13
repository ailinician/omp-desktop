# omp extension widgets in the rail

User-facing behaviour: [`docs/widgets.md` § omp extension widgets](../widgets.md#omp-extension-widgets). This file covers the wire frames, where the state lives and why. Issue #52.

## Code

| Path | Role |
|---|---|
| `src-tauri/src/agent/ext_ui.rs` | `ExtUiState`: a session's latest status lines and widgets, bounded; `apply(frame)` |
| `src-tauri/src/agent/reader.rs` | applies every `extension_ui_request` stdout frame to the session's `ExtUiState` before the journal push |
| `src-tauri/src/agent/mod.rs` | `AgentBridge::extension_ui_state(session_id)`; the state lives on `BridgeInner` |
| `src-tauri/src/lib.rs` | `extension_ui_state` command |
| `src/app/extension-widgets.js` | pure: `apply`, `fromSnapshot`, `isFireAndForget`, `cards`, `toggles` (`window.OMP_EXT_WIDGETS`); test `tests/test-extension-widgets.mjs` |
| `src/live.js` | per-tab `state.extWidgets`: live frames, snapshot/restore, the dropped-replay load, lifecycle |
| `src/app-live.jsx` | the cards (after the plugin cards) and the Tweaks → Rail rows |
| `src/design/widgets/widget-card.jsx` | `WidgetCard`'s `source` prop: chip + `wdg-pushed` accent border instead of run timing |

## Wire frames

omp's RPC UI context (`packages/coding-agent/src/modes/rpc/rpc-mode.ts`) writes these as ordinary stdout lines, each with an `id`, and waits for no reply:

| Method | Fields | Removal |
|---|---|---|
| `setStatus` | `statusKey`, `statusText` | `statusText` absent (`undefined` is dropped by `JSON.stringify`) |
| `setWidget` | `widgetKey`, `widgetLines: string[]`, `widgetPlacement?: "aboveEditor" \| "belowEditor"` | `widgetLines` absent |
| `notify`, `setTitle`, `set_editor_text` | not shown by the desktop | — |

- Statuses and widgets are separate key spaces.
- A widget built as a component factory is not forwarded at all, so a key that switches from lines to a factory keeps its last lines.
- `widgetPlacement` is a TUI layout hint; the rail ignores it.
- Lines are usually plain (chalk is level 0 when stdout is not a TTY) but may carry SGR and OSC 8; `extension-widgets.js` strips CSI, OSC, other ESC sequences and C0/C1 controls except tab and newline.
- `select`, `confirm`, `input`, `editor` and `ask` are the awaited methods. Any method the desktop has no UI for still gets `{cancelled: true}` unless `isFireAndForget` names it: a reply to a fire-and-forget id is noise omp ignores.
- In the TUI since v0.31 (`setStatus`) / v0.34 (`setWidget`), under `extension_ui_request` since v0.35: no `MIN_OMP_VERSION` change.

## State and bounds

`{ statuses: [{key, text}], widgets: [{key, lines, more}] }`, keys in first-appearance order; replacing a key keeps its place (omp's TUI uses a `Map`). Same bounds on both sides, so a snapshot and the live frames give the same cards:

| Bound | Rust (`ext_ui.rs`) | JS (`extension-widgets.js`) |
|---|---|---|
| keys per kind | 32, further keys ignored until one is removed | same |
| key length | 200 UTF-8 bytes, longer ignored | same (counted in UTF-8) |
| lines per widget | 50; `more` counts the string lines past it | same; `fromSnapshot` adds the backend's `more` |
| text per status / line | 2000 bytes, cut on a char boundary | 500 chars after stripping, `…` |

Non-string lines are skipped; a non-array `widgetLines` is a removal.

## Why a copy in Rust

The frontend processes only the active tab's frames; a background tab's lines wait in its event journal (`agent/journal.rs`, 256 lines / 8 MiB) until activation replays them. One streamed turn overflows that ring, and omp has no command to query its current widgets. Without the backend copy, every `setStatus`/`setWidget` among the evicted lines would be lost, and a tab would come back after a background turn with stale or missing cards.

So the stdout reader applies each `extension_ui_request` frame to the session's `ExtUiState` (a type check on the frame it already parsed; other frames cost nothing more), before pushing the line to the journal. On activation (`_switchToSession`), when `replay_events` reports `dropped`, the frontend replaces `state.extWidgets` with `fromSnapshot(extension_ui_state)` and then drains the live lines buffered since its listener attached. The snapshot is at least as new as any buffered line, and set/remove per key is idempotent, so re-applying those lines in order converges on the backend's state. Without `dropped`, the replay alone is complete and no snapshot is fetched.

The backend sees whole lines only: a frame omp splits into protocol-v2 chunks (over 1 MiB, far beyond the bounds above) reaches the frontend live but not `ExtUiState`.

## Lifecycle

omp never sends a clear or re-send on a session change, and its TUI keeps extension widgets through a user's `/new`, `/resume`, branch and fork (`clearHookWidgets` runs only around an extension-initiated `newSession`/`switchSession`; statuses always persist). The desktop matches the user-initiated case:

- `_reloadConversation` (`new_session`, branch, fork) carries `state.extWidgets` across its `_resetSessionVars`.
- `_endDeadProcessState` (exit, startup death) clears it. The Rust copy lives on the session's `BridgeInner`, which a process exit, `stop_session` or a respawn under the same id (profile switch) removes or replaces, so `extension_ui_state` is empty for it.
- Tab switches save and restore it with the rest of the per-tab state.

## Rendering

`cards(ext)` returns `WidgetCard` models: a `status` card (id `ext-status:`, tweak `widget:ext-status`) whose block is `kv` with a row per status, then one card per widget with lines (id `ext:<key>`, tweak `widget:ext:<key>`) whose block is `text` with `mono: true`, plus a `… N more lines` line. The ids contain a colon, which plugin ids (`[a-z0-9-]`) cannot, so React keys never collide with plugin cards. `mono` is an internal block field: `parseOutput` never produces it from plugin output.

The run is always `{state: "ok", widget: {status: "neutral"}}` with no timing; `WidgetCard`'s `source="extension"` replaces the timing chip and adds the `wdg-pushed` accent border. No refresh button: nothing runs. A hidden card (`t[tweak] === false`) is only not rendered; its state keeps updating, and `widgets_set_view` never sees these keys (`hiddenPluginIds` reads `widget:plugin:` keys only).
