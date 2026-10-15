# Browser panel

One desktop tab owns one headless Chromium, shared by its omp agent and the browser panel. Rust owns the choice of browser, the process, the CDP connections, page selection, the screencast and the annotation overlay. The app webview only paints images and forwards input: browser pages never load in it.

## Modules and data flow

| File | Responsibility |
|---|---|
| `src-tauri/src/browser/mod.rs` | `BrowserManager`: per-tab entries (placeholder proxy, launch `Slot`, overlay path, panel slot), allocation, parallel `release_many` / `shutdown_all`, startup pruning entry point |
| `browser/process/` | `settings.rs` (`browser.json` view over `program_setting`, `browser_*` settings payload), `locate.rs` (candidate discovery per platform, snap skip), `launch.rs` (`Chromium`: flags, environment, fallback to the next candidate, stop), `stderr.rs` (bounded stderr tail for launch errors), `instance.rs` (private instance dir, overlay, exclusive `0600` files, dead-instance pruning, retried removal), `reap.rs` (verified orphan kill) |
| `browser/proxy/` | Agent-facing CDP proxy: `mod.rs` (accept loop, connection cap, splice), `http.rs` (request admission, `/json*` relay and URL rewrite), `feed.rs` (framing on the pipe threads, bounded queue, observer thread), `tap.rs` (intent tracking), `ws_frames.rs` (WebSocket decoder); suite in `tests.rs` |
| `browser/cdp.rs` | Browser-level CDP client, flattened page sessions, request/reply matching, fire-and-forget `post`, disconnect notification |
| `browser/view/` | `View` (`mod.rs`: open, navigate with URL allowlist, history, page tabs, input), `core.rs` (shared state, screencast, title refresh), `switching.rs` (tab switch, agent-follow), `frames.rs` (frame flow control), `worker.rs` / `events.rs` (CDP events, follow and hold), `input.rs` (`InputEvent`), `model.rs` (pages, sizes, hold), `emit.rs` (event payloads) |
| `browser/annotate/` | `mod.rs` (overlay install, `set_annotate`, release), `pick.rs` (binding calls, pick validation), `capture.rs` (clipped PNG capture, scroll-to), `overlay.js` |
| `browser/commands.rs`, `src-tauri/src/lib.rs` | Panel commands; settings commands, session allocation, startup pruning thread and app shutdown integration |
| `src/live.js` | Browser command/event bridge, settings bridge (`browserSettings`, `browserSetPath`, `browserPickPath`), release on desktop-tab close |
| `src/app/browser.js` | Pure URL, input, Esc, coordinate, chip, message-merge, width and Chromium-menu helpers |
| `src/app/browser-screen.js`, `src/app/use-browser.jsx` | Canvas decoding/painting, frame acks, DOM input through a focus sink; view lifecycle, events, per-tab chips and chat localhost-link routing (`onChatClick`) |
| `src/design/browser-{dock,panel,toolbar,chips,tweaks}.jsx`, `src/design/layout/browser.css` | Resizable/full-view dock (`ui/use-edge-drag.js`, shared with the terminal), page tabs, controls, annotation popover and chips; Tweaks → Browser `chromium` row (`design/path-setting.jsx` + `app/path-setting.js`, shared with the terminal's shell) |

```text
start_session -> BrowserManager.allocate -> private settings overlay
             -> omp --config=<overlay> (browser.cdpUrl = placeholder URL, relay off)

omp browser tool -> loopback placeholder proxy -> Chromium CDP
                            |
                     read-only tap -> last agent target -> panel view

app controls -> live.js -> Tauri browser_* commands -> view -> Chromium CDP
Chromium screencast / state / picks -> browser:// events -> useBrowser
                                   -> canvas / toolbar / annotation chips
chips + draft -> app-live.jsx -> omp prompt (text + captured images)
```

The panel connects directly to Chromium's own debugging port, not through the proxy. Its own input and screenshots therefore do not look like agent intent to the tap.

## Which Chromium

The browser is a Rust-owned setting, like the terminal's shell: `<app config>/browser.json` `{ "path": "" }` through `json_store` (`process/settings.rs`). The webview never sends a path for a launch and keeps no copy. Tweaks → Browser shows a `chromium` row (`design/browser-tweaks.jsx`) whose menu lists `automatic` (with what discovery finds now), every browser discovery finds, the stored path when it is none of them, and "other…":

- `browser_settings()` → `{ path, automatic, installed, error }` (`path` `""` = automatic; `error` when the stored path is no longer an executable file).
- `browser_set_path(path)`: trims, drops one pair of surrounding `"` (Windows "Copy as path"), stores `""` or an absolute path to an executable file (`program_setting::is_executable_file` / `typed_path`, shared with the terminal's shell); a macOS `.app` bundle stands for `Contents/MacOS/<name>`.
- `browser_pick_path()`: native file dialog in Rust (as `terminal_pick_shell`), stored as above; `null` = cancelled.

`Slot::ensure` reads the setting at every launch, so a change applies to the next Chromium a tab starts; a running one is not replaced. A configured path that no longer works is a launch error (`browser://state` `error`, HTTP 503 to the agent), never a fallback to another browser.

Automatic discovery (`process/locate.rs`, a pure function of the platform and its places, so every platform's order is tested on any host):

- **Linux:** real browser binaries first (`/opt/google/chrome/chrome`, `/usr/lib/chromium/chromium`, `/usr/lib/chromium-browser/chromium-browser`, `/usr/lib64/chromium-browser/chromium-browser`, `/usr/lib64/chromium/chromium`, Chrome Beta/Unstable, `/opt/microsoft/msedge/msedge`, `/opt/brave.com/brave/brave`), then omp's Chrome for Testing, then the PATH launchers (`google-chrome`, `google-chrome-stable`, `chromium`, `chromium-browser`, `microsoft-edge`, `brave-browser`) last. Launchers are distro scripts that inject the user's own flag files; Chrome for Testing goes after the installed browsers because Ubuntu 23.10+ refuses it a sandbox and the desktop keeps the sandbox on. A candidate that resolves into `/snap/`, to the `snap` binary, or is a small `#!` script mentioning `/snap/` is skipped: snap confinement keeps it out of the hidden cache folder its profile lives in. The skip reason is part of the "no Chromium found" error.
- **macOS:** Chrome for Testing first, then Google Chrome, Chromium, Edge, Brave, Chrome Beta and Canary under `/Applications` and `~/Applications`.
- **Windows:** Chrome for Testing first, then Chrome, Edge, Brave, Chromium and Chrome Beta under `%ProgramFiles%`, `%ProgramFiles(x86)%` and `%LOCALAPPDATA%` (per-user installs).

Chrome for Testing builds come from omp's puppeteer cache, newest version first: `$XDG_CACHE_HOME/omp/puppeteer/chrome` when that `omp` folder exists (not on Windows), then `~/.omp/puppeteer/chrome`. Each file is tried once even when several names reach it. The app does not download a browser.

A launch tries the candidates in order, at most three: a binary that cannot run or exits before publishing its debugging port makes way for the next; a timeout or a pid-file failure stops the attempt. The error lists every attempt with the last 2 KiB of its stderr (`process/stderr.rs`; a thread drains the pipe for the browser's whole life).

## Placeholder port, overlay and lazy launch

`start_session` allocates a browser entry before spawning omp. Allocation binds `127.0.0.1:0` and exclusively creates `<app cache>/browser/<pid>/<session_id>.yml` containing only:

```yaml
browser:
  cdpUrl: "http://127.0.0.1:<placeholder port>"
  relay: false
```

`relay: false` because omp ranks an enabled browser relay above `cdpUrl`; without it a user with the relay on would drive their own Chrome, not the panel's browser. omp's Tern setting ranks below `cdpUrl` and needs no override.

`agent/spawn.rs::omp_args` appends the single token `--config=<path>` **last**, after profile/resume/cwd and `--approval-mode=<mode>` (the mode `ApprovalModeStore::resolve` picked). Preserve this overlay order and its browser-only contents: the tab must use the managed endpoint without replacing its normal profile settings or changing approval policy.

Allocation does not launch Chromium. The first admitted proxy request or `browser_open_panel` calls the tab's `Slot::ensure`. Its per-tab lock prevents duplicate launches without blocking other tabs. The placeholder port stays stable across Chromium restarts; `/json*` discovery replies rewrite upstream loopback WebSocket URLs to that port. WebSocket upgrades and traffic are otherwise spliced byte for byte. Launch failure returns HTTP 503; an upstream connection failure returns 502.

Only an app run without managed browsers at all (no cache dir: `BrowserManager::unavailable`, logged once at startup) starts omp without the overlay; omp then uses its own browser configuration and the panel has none. A tab whose own allocation fails (a respawn racing the tab's close: "still closing", the port or overlay failing) fails `start_session` (`allocate_if_available`), and the frontend's rollback closes the tab: it never runs silently without its browser.

## Proxy admission

`proxy/http.rs::admit` vets every request head before anything launches; a refusal is HTTP 403 and never calls `ensure`:

- an origin-form request target (starting with `/`);
- well-formed header lines and exactly one `Host` naming `127.0.0.1`, `localhost` (case-insensitive) or `[::1]`, with an optional numeric port (DNS rebinding);
- no header only a web browser sends: `Origin`, or any `Sec-Fetch-*` header except `Sec-Fetch-Mode` on its own (Node's `fetch` sends that one, and puppeteer may run there; a browser always adds `Sec-Fetch-Site` too);
- outside `/json*`, an `Upgrade: websocket` request.

Only `GET /json*` is relayed with rewritten URLs; other admitted requests are spliced unchanged. At most 32 connections are served at once: one more gets a 503 written by the accept thread, without a thread of its own.

## Launch flags and environment

`process/launch.rs` starts Chromium with:

- `--headless=new`: rendering without a separate browser window.
- `--remote-debugging-port=0`: Chromium chooses a free loopback CDP port, read from the fresh profile's `DevToolsActivePort` (10-second startup timeout).
- `--user-data-dir=<private throwaway profile>`: no reuse of the user's normal browser profile.
- `--no-first-run`, `--no-default-browser-check`, `--disable-background-networking`: suppress first-run/default-browser flows and background browser networking, not page networking.
- `--disable-extensions`: no extension, not even one a launcher's flag file or a managed policy adds.
- `--window-size=1280,800`: initial size; opening/resizing the panel applies CDP device metrics with its CSS width, height and DPR.
- `--use-mock-keychain --password-store=basic`: keep this disposable profile away from the OS keychain/keyring, avoiding repeated macOS “Chromium Safe Storage” prompts. This is not an encrypted persistent credential store.
- `about:blank`: initial page.

The child's `XDG_CONFIG_HOME` points at an empty folder inside the profile and `CHROME_USER_FLAGS` / `CHROMIUM_FLAGS` are removed, so a distro launcher (the last-resort candidates) finds none of the user's `*-flags.conf` files or flag variables. Child PATH resolution uses `child_path::apply_child_path`; process-tree supervision uses the same process-group/Windows Job Object machinery as agent children. No `--no-sandbox` flag is added.

## Agent-follow: intent, not attachment

Each agent proxy connection has a read-only tap. For a page-level WebSocket it knows the target from `/devtools/page/<id>`; for a browser-level connection it maps flattened `sessionId` values to page `targetId` values from attach replies/events. Non-page sessions are excluded when identified by the event.

Follow signals are `Target.activateTarget`, successful `Target.createTarget`, `Page.navigate`, `Page.reload`, `Page.navigateToHistoryEntry`, `Page.bringToFront`, `Page.captureScreenshot` and `Input.*`. **Attachment is bookkeeping, not intent**: Puppeteer can remain attached to every page, so following `Target.attachedToTarget` or an attach reply would pick arbitrary pages rather than the page the agent actually uses.

The entry remembers the last reported target even while the panel is closed. Opening starts there if it still exists, otherwise on the newest discovered page (creating `about:blank` if needed). The user's own actions hold the view for five seconds: picking, opening or closing a page tab, address-bar navigation, back/forward, and any panel input except pointer moves. While held, the newest agent report is kept and shown when the hold ends; the decision is re-checked under the switch lock, so a tab the user picks meanwhile wins. A report for a page not listed yet waits for its discovery; a closed page is dropped. An agent report while annotate mode is on turns annotate off (the user's picks would land on a page the agent is changing).

Framing runs on the pipe threads: each direction's `Feed::push` decodes the chunk **before** forwarding it and queues only complete text messages under the size cap (commands 1 MiB, replies/events 64 KiB; a larger payload such as a screenshot reply streams past without being copied). Both directions share one queue (at most 1024 messages and 4 MiB per connection), so commands, replies and the commands depending on them reach the observer in causal order. JSON parsing and the follow callback run on a separate observer thread that the splice **never waits for**. A full queue, malformed framing, failed upgrade, negotiated WebSocket extension (including permessage-deflate), or observer failure disables tapping for that connection and logs why; forwarding continues unchanged. The tap must never block or alter the splice.

## View, frames and input

A view uses one browser-level CDP socket with `Target.attachToTarget { flatten: true }`. It discovers page targets and attaches to the page being shown. A switch releases the old overlay, stops its screencast and detaches it, then enables the new page and installs its overlay; a failure after the attach sets `state.error` instead of leaving a frozen frame. `tab_activate` / `tab_close` refuse ids not in the view's page list ("no such tab"). Page titles are refreshed with `Target.getTargetInfo` after a switch and on `Page.loadEventFired` / `Page.navigatedWithinDocument`, so a tab shows the document title, not the URL it had when the navigation committed.

Viewport changes restart JPEG screencasting (quality 70). The panel size is validated before Chromium is launched for it: 1–8192 CSS pixels per side, DPR 0.25–8, at most 8192 device pixels per side.

**Panel channel** (`view/emit.rs`): `browser_open_panel` takes a Tauri `Channel` from the webview, and everything the view reports during that open goes over it, in order: `state`, `tabs` and `pick` as JSON `{ type, value }`, frames as raw bytes. A frame is a 16-byte little-endian header (`seq` u64, `cssW` u32, `cssH` u32) followed by the JPEG, decoded from Chromium's base64 once in Rust on the emitter thread, so the webview parses no base64 or JSON per frame (large payloads reach it through Tauri's channel fetch, as an `ArrayBuffer`). One channel per open replaces per-kind event listeners: there is no listen/unlisten race on open or close, and a page's `tabs` message precedes its frames because both share the channel's order. The reader passes CDP event params by value, so a frame's base64 moves from the socket to the emitter without a copy.

**Frame flow control** (`view/frames.rs`): every frame carries a per-view `seq`. At most one frame is in flight to the webview; the newest frame waits in a one-frame slot and replaces an older waiting one. Chromium's `Page.screencastFrameAck` is sent only once the webview acks the frame it holds (`browser_frame_ack { sessionId, seq }`, synchronous and cheap) or after a 1-second timeout, so a busy webview slows Chromium down instead of queueing frames, and a lost ack costs a second, not the stream. Skipped frames are acked to Chromium on the next release. Emitting a frame takes a dedicated emit-order lock (also held while a switch publishes its tabs), never the view's model lock while decoding or sending. Activation generations reject old-page frames and queued input after a page switch.

`browser-screen.js` decodes JPEGs with `createImageBitmap`, decodes only the newest frame and paints on a DPR-sized canvas. Every frame is acked exactly once: after it was painted, or when it was dropped (superseded before decode or paint, undecodable). Frames carry the page CSS dimensions they show; marker geometry is published with the bitmap actually painted. A size reaches Rust once it held for 100 ms, from a `ResizeObserver` or a `matchMedia` resolution query (moving the window to a display with another scale). Pointer coordinates map through frame CSS dimensions rather than canvas backing pixels. Mouse moves and wheel ticks are coalesced to one of each per animation frame (wheel deltas add up), mapped to page coordinates when they go out, and flushed before a press/release; side buttons 3/4 are `back` / `forward`. Rust's synchronous `browser_input` only queues work (at most 512 events), preserving IPC input order; the input thread posts each event without waiting for Chromium's reply (CDP keeps a session's order), so input is not capped at one event per round trip. Other commands run blocking CDP work off the IPC thread. Commands whose reply nothing reads (stopping the old screencast, detaching, `activateTarget`, focus emulation, the overlay's `Runtime.enable` and binding) are sent without waiting as well. `loading` follows the shown page's main frame only (its frame id is the target id); subframes loading do not flip it. Refreshing a frame (annotate toggle, after a capture) restarts the screencast without re-applying the viewport.

`browser_navigate` loads only `http://`, `https://`, `data:` and `about:blank` URLs (Rust's allowlist, matching the address field's `normalizeUrl`); anything else is "unsupported url scheme". It returns once `Page.navigate` is sent: a slow server is not a failure, and a failed load shows as `state.error` ("could not load <url>", from `frameNavigated`'s `unreachableUrl`) until a later navigation succeeds.

Keyboard focus sits on a hidden `<textarea>` (`.browser-sink`) inside the view, so paste, IME composition and dead keys work: keys that type nothing go out as CDP key events, composed text, dead-key characters and pastes as `{ kind: "text" }`. Logical letter keys determine virtual key codes on non-US layouts; AltGr (`getModifierState("AltGraph")`) types its character and drops the Ctrl/Alt bits. Which keys leave the page for the app is the terminal's rule, `OMP_KEYMAP.guestShouldBypass` (`app/keymap.js`): a chord bound to a global action marked `fromGuest` (`desktop.browser.toggle`, `desktop.terminal.toggle`, next/previous tab), a `super` chord, or off macOS a `ctrl+shift` chord goes to the app; plain `ctrl+<key>` / `alt+<key>`, unbound chords, the copy/paste chords and AltGr characters stay with the page. Esc while annotating is the panel's: it closes the comment popover first, then leaves annotate mode, and is always prevented, so the keymap's interrupt never fires from the panel; outside annotate mode Esc goes to the page. `useBrowser` installs event listeners before opening the view, queues initial URL/resize requests until opening settles, and disposes listeners, bitmaps and the view when leaving the panel or switching desktop tabs. A refused `browser_input` (no page shown) is logged at most once a minute per open panel.

## Annotation overlay and binding contract

`annotate/` installs `overlay.js` **per attached page session**, not just once per browser. `Runtime.addBinding` exposes `__ompAnnotate` only in the isolated execution-context world named `omp-annotate`. `Page.addScriptToEvaluateOnNewDocument` uses that world and `runImmediately: true` for the existing document and future navigations. Context-created events apply the current annotate flag; release forgets contexts and turns the overlay off before detach.

`set_annotate` updates the flag first, installs the overlay only when turning on, and evaluates the active page's contexts on the way off only if there is a page, so turning off fails only for a closed view. The flag is reported as `browser://state` `annotate` (Rust's truth; the toolbar shows it; a closed view reports `false`), and every toggle forces a fresh screencast frame so no stale hover outline stays on screen.

The script ignores subframes, uses a closed-shadow overlay host and installs capture-phase pointer/mouse listeners (including enter/leave). Annotate mode suppresses page click/hover handling but leaves scrolling and keys available. Only trusted events are swallowed: a page script's or the agent's `el.click()` still reaches the page. A left click picks an element; a drag over four CSS pixels picks an area (a drag with a side under 1 px becomes an element pick). Rendering must stay **CSSOM-only** (`element.style`, `textContent`): no style elements, inline-style attributes or HTML strings. This keeps the overlay working with strict page CSP and Trusted Types.

`__ompAnnotate(JSON.stringify(payload))` carries:

- `kind`: `element` or `area`.
- `rect`: viewport CSS pixels `{ x, y, w, h }`; `docRect`: the same box plus page scroll at pick time (required).
- `url`: page URL.
- Element-only `selector`, `tag`, `html`, `text`; areas carry null element fields.

Selectors prefer a unique escaped id, then unique `data-testid`, then at most six ancestor segments. Names use `localName` (SVG elements keep their case); tags and classes are CSS-escaped; `:nth-of-type` disambiguates same-tag siblings still matched by the tag/class segment. HTML/text are bounded to 20,000/500 UTF-16 units, URL/selector/tag to 8192, without splitting surrogate pairs (`toWellFormed()` where available). Rust accepts only the named binding from a tracked isolated-world context of the active page, and only within two seconds of a mouse release the panel itself sent (`panel_released_within`), so agent input never becomes a user pick. It bounds the JSON payload to 256 KiB and the fields, and rejects invalid/non-finite rectangles, areas under 1 px or unknown kinds. Page scripts cannot directly call the isolated binding, but DOM content remains untrusted.

`browser_capture` takes the pick's `docRect` and only works on the document the last pick came from: after a navigation or page switch it fails with "the page changed since the pick". It hides the overlay **visually only** (`__ompAnnotateHide`, counted, so concurrent captures each hide and show it; annotate stays on and clicks stay swallowed) in the pick's isolated context, which also returns scroll, viewport size and device pixel ratio (the page cannot spoof them there). The clip is `docRect` cut to the visible viewport (an empty result is an error), scaled so its longest side stays within 2000 device pixels (`OMP_IMAGES`' edge cap). The overlay is shown again whatever the outcome; a failed show does not discard a good shot. A failed capture still permits a text annotation and shows a failure note. Chips retain `docRect` for scrolling back to the pick when the panel still shows the same URL.

## Commands and events

Every command identifies the desktop tab with `sessionId` (not the Chromium page `targetId`). Commands are registered in `lib.rs`; panel commands live in `browser/commands.rs`.

| Command | Additional arguments / result |
|---|---|
| `browser_settings` | Returns `{ path, automatic, installed, error }` |
| `browser_set_path` | `path` (`""` = automatic); returns the settings |
| `browser_pick_path` | Native dialog; returns the settings or `null` |
| `browser_open_panel` | `width`, `height`, `dpr`, `events` (the panel channel); ensure Chromium, discover pages, start view |
| `browser_close_panel` | Stop/detach view; leave Chromium alive |
| `browser_resize` | `width`, `height`, `dpr` |
| `browser_navigate` | `url` (allowlisted; toolbar reload navigates to the current URL) |
| `browser_history` | Signed `delta` |
| `browser_tab_new` | No additional arguments; opens `about:blank` |
| `browser_tab_close`, `browser_tab_activate` | `targetId` (must be listed) |
| `browser_input` | Tagged `event`: `mouse`, `wheel`, `key`, or `text` |
| `browser_frame_ack` | `seq` of a painted or dropped frame |
| `browser_annotate` | Boolean `on` |
| `browser_capture` | The pick's document `rect`; returns `{ data, mimeType: "image/png" }` |
| `browser_scroll_to` | Document `rect` |

| Channel message | Payload |
|---|---|
| frame (raw bytes) | `seq` u64, `cssW` u32, `cssH` u32 (little-endian), then the JPEG |
| `{ type: "tabs", value }` | `[{ targetId, title, url, active }]` |
| `{ type: "state", value }` | `{ status, error, url, loading, canBack, canForward, annotate }`; status `idle`, `starting`, `running`, `stopped`, or `error` |
| `{ type: "pick", value }` | Pick payload above |

## Panel, links, chips and the prompt

The panel takes the column beside the chat, which `app-live.jsx` keeps per desktop tab as one value: the rail, the conversation tree or the browser (`side`). The stage layout, the widgets' `railVisible`, the tree's fetching and `useBrowser`'s `open` all derive from it, so opening one closes the other and a tab switch shows that tab's column. Leaving the browser leaves full view.

A primary click on a chat link to `localhost`, `127.x.x.x` or `[::1]` (`OMP_BROWSER.linkClick`) opens it in the panel; with ⌘/Ctrl, Shift or Alt it goes to the system browser through the external opener. The markdown containers route it: their delegated click (`useMarkdownClick`, `ui/markdown.jsx`) runs the copy buttons, then the handler `MarkdownLinkContext` carries, which `app-live.jsx` provides as `browser.onChatClick` (null while the Tweaks toggle is off).

Saved chips are per desktop tab, held by `useBrowser` (the panel draws their markers on the page they were picked from). A chip's screenshot goes through `OMP_IMAGES.normalizeImage` like a pasted image. Chips go out with a submit like the rest of the composer's draft: `app-live.jsx` takes them at once (`takeChips`: those ids; chips added meanwhile stay for the next message) and merges each chip's comment, URL, selector and bounded HTML into the send, steer, follow-up or goal delivery (`OMP_BROWSER.mergeIntoMessage`), adding their PNGs subject to the shared eight-image cap. Excess screenshots are reported, while annotation text still goes out. Like typed text and images, a delivery omp refuses is not handed back (a refused steer or follow-up shows the whole message in its failure note); a goal start omp refuses hands its draft back, chips included (`restoreChips`). A goal stores only the typed objective; the chips ride in the objective's delivered prompt or steer (`goalStart`'s `prompt` argument, `docs/architecture/goal-mode.md`).

Where a submit goes is decided on the typed draft (`OMP_SLASH.routeSubmit`), before annotations merge in; a command carries no chips. Merging never changes that classification, so `live.js` `steer` / `followUp` classify the text they send as for any other caller. Page-derived fields are labeled as untrusted data and wrapped with `OMP_MARKDOWN.fenceCode`, using a backtick fence longer than any run inside those fields. `/skill:` tokens and line-leading slash commands are escaped in the page fields and in the user's comment (which stays outside the fence) because omp's command parser does not honor Markdown fences.

## Lifecycle

- Startup prepares `<app cache>/browser/<pid>/`. Unix directories are created with `0700`, overlay and pid files with `0600` and exclusive creation (`instance.rs::create_private_file`); session ids are restricted before becoming filenames. The cache root and instance path must be real directories, not symlinks. A directory with the app's own pid that already exists is a crashed earlier run's: its browsers are reaped and it is recreated empty. Each launch attempt uses `profiles/<session_id>-<n>` and records its browser PID next to the profile as `<profile>.pid`, removed after reaping.
- Dead instances are pruned on a `browser-prune` thread at startup (`BrowserManager::prune_stale_instances`), never on the main thread. A directory is dead when its pid is not alive (`supervisor::pid_is_alive`: `kill(pid, 0)` on Unix, `OpenProcess` + `GetExitCodeProcess` on Windows); other live instances and non-pid names are left alone. On macOS/Linux the recorded browsers are reaped first: a PID is targeted only if it is alive, its command line contains `--user-data-dir=<profile path>` bounded by start/end, NUL or ASCII whitespace, and it leads its own process group (`supervisor::kill_group_if_leader`, which then SIGKILLs the group). Linux reads `/proc/<pid>/cmdline`, accepting both NUL-separated argv and Chromium's rewritten space-joined process title; macOS reads `/bin/ps -o command= -p <pid>`. Cleanup waits up to two seconds for the leader to disappear; if it remains alive, the directory is retained rather than deleted under a running browser. On Windows the Job Object already killed every browser of a dead process, so its directory is removed directly.
- Profile and instance removal retries on Windows (10 × 100 ms) while a killed Chromium's helper processes still hold files.
- Closing the panel or switching desktop tabs ends only the view. Browser pages continue running; the tab's side column and chips remain per tab in React memory.
- An omp profile switch or approval-mode restart respawns the agent with the same tab overlay and browser. `stop_session` must not release Chromium.
- After a Chromium crash the view reports stopped. Panel reload/reopen or the next agent connection launches a fresh throwaway profile behind the existing placeholder port; old page state is lost.
- Desktop-tab close calls `live.js::_closeTab` / `close_tab`; reload cleanup uses `close_all_tabs`. Both release each tab's browser as well as its agent, widgets, git watcher and terminal. Only `close_tab` / `close_all_tabs` and app-wide `kill_child_processes` end a tab's browser. Release marks the slot closed, closes the view and proxy sockets, waits out an in-flight launch, kills/reaps Chromium's process tree, and removes its profile and overlay. Concurrent release callers wait for teardown; shutdown includes releases already in flight. `close_all_tabs` releases its tabs' browsers side by side (`release_many`), and browser teardown runs alongside the terminals' hang-up grace (`lib.rs` `end_tabs`).
- `BrowserManager::Drop` calls `shutdown_all`, which tears every entry down in parallel (app exit waits for the slowest teardown, not their sum) and removes this process's `<root>/<pid>` directory. App exit and updater relaunch explicitly call `kill_child_processes`, killing agent trees first, browsers second, widget plugins third and terminal shells last; these paths cannot rely on managed-state destructors running.

## Security model and known limits

The proxy and Chromium CDP endpoint are loopback-only, not network services. **Any local process of the same user can reach the CDP port.** There is no CDP authentication or per-process authorization; loopback is not a boundary against other local processes. Private files/profiles protect their filesystem contents, not socket access. Proxy admission keeps web pages out (loopback `Host`, no `Origin` or browser fetch metadata, WebSocket upgrades only outside `/json*`), not local programs. Do not expose or tunnel either debugging port.

Agent actions still go through omp's normal tool approval flow under the tab's resolved `--approval-mode=<mode>`: with `yolo`, the agent's browser actions run without asking. The desktop proxy does not approve, filter or reinterpret tool calls. Manual panel controls are direct user actions, not agent tool calls. Chromium executes remote pages separately from the Tauri app webview: the webview receives image frames and bounded pick metadata, never page scripts. Pick HTML is prompt text, not app markup. Screenshots, DOM snippets and URLs may contain sensitive page data; sending an annotation intentionally includes that data in the agent prompt.

Limits:

- **The agent drives the page you see.** omp treats a `cdpUrl` browser as a borrowed, user-driven one: it never opens a tab of its own there and works in the visible page, so its navigation replaces the page you were on. Nothing in the desktop can give it a separate tab.
- One Chromium per desktop tab that used the browser, running until that tab closes: no cap and no idle shutdown.
- omp waits 5 seconds for the CDP endpoint, while a cold Chromium launch may take up to the 10-second startup timeout; a slow first launch can fail the agent's first browser call, and the next one finds Chromium running.
- An explicit `app.relay` / `app.tern` / `app.cdp_url` in a tool call and the `PI_BROWSER_RELAY` environment variable still outrank the overlay: the agent then drives a different browser than the panel shows.
- Requires a compatible installed Chromium-based browser or omp's Chrome for Testing; snap and flatpak packages are not candidates. No bundled browser or downloader.
- Fresh profiles mean no normal-browser cookies/logins/extensions and no browser-state persistence across tab close, crash or app restart. A browser configured in Tweaks that is itself a wrapper script still runs whatever that script adds. Pending annotation chips and panel-open state are in-memory only.
- Headless screencast/input is not a native browser surface: no native browser chrome, file-picker/download management or audio forwarding. Text input arrives as plain text (paste, IME and dead-key results).
- Annotation targets are the top-level DOM, not elements inside iframes or shadow roots. Selectors are bounded best-effort hints; page mutation can invalidate them.
- Saved boxes are snapshots, not live DOM tracking. Markers use pick-time viewport coordinates and match by URL; chip scrolling uses pick-time document coordinates only on the currently shown matching URL, without reopening a page or selecting its original target.
- Follow is limited to observed intent commands through the proxy. WebSocket extensions, an observer stuck until the queue fills or unsupported framing can disable it for a connection without affecting agent traffic (logged to stderr only). Local processes using the proxy are indistinguishable from the agent to the tap.
- A spliced WebSocket has no idle timeout: a half-open connection keeps its threads and one of the 32 connection slots until the tab's browser is released (`Proxy::shutdown` cuts every live socket).
- Orphan identity checks cannot recover original argument boundaries from Linux's rewritten process title or macOS `ps` output. `--user-data-dir=.../s1-0 x` matches `.../s1-0` because space is a delimiter, unlike `.../s1-01` or `.../s1-0-other`. Generated session/profile basenames contain no spaces, but the OS-provided cache root (including a macOS home directory) can contain spaces; there is no enforced space-free full-path invariant. Matching includes any spaces inside the requested full path, but cannot distinguish a separate argument from a different profile path with an appended whitespace suffix. Reaping remains best-effort, not a guarantee of exact argv identity.

## Tests

Rust: `browser::process` (candidate order per platform, snap skip, settings validation, fallback and stderr, child flags and environment), `browser::proxy` (admission never launches, connection cap, large replies keep follow, order), `browser::view`, `browser::annotate` (strict CSP, main world cannot see the binding, unowned contexts, navigation/tab switching, clean/scrolled/concurrent capture, viewport clip and pixel budget, Unicode boundaries, SVG and escaping) and `browser::tests` (manager, shared fixtures). JS: `tests/test-browser.mjs` (`app/browser.js`, submit routing) and `tests/test-browser-runtime.mjs` (`browser-screen.js`, `use-browser.jsx` against stubs) in `npm test`.

Real-Chromium Rust tests find a browser with `browser::tests::chromium_or_skip`: without one they pass with a note, unless `OMP_REQUIRE_CHROMIUM=1` (set in CI) turns that into a failure. They acquire the shared, poison-tolerant `browser::tests::CHROMIUM` mutex before constructing their fixtures and hold it through browser teardown, which serialises browser lifetimes across suites even when libtest runs many worker threads; fake-browser and pure-logic tests stay parallel. Input tests focus their target explicitly via CDP and wait for `document.activeElement` rather than relying on autofocus.
