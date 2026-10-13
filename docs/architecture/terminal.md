# Terminal panel

A real shell per tab in a panel above the composer, in the tab's project folder. **Rust owns the terminal**: `src-tauri/src/terminal/` keeps the tab registry, picks the working directory and the shell, persists the shell setting, spawns and kills the process tree, numbers and buffers the output and throttles a shell the webview cannot keep up with. The webview is a renderer that sends a tab id, keystrokes and view state: `src/app/terminal.js` (`window.OMP_TERMINAL`, regression `test-terminal.mjs`) holds the pure panel state and key routing, `src/app/use-terminal.jsx` owns one xterm.js instance per tab and keeps it in sync with Rust, `src/design/terminal-panel.jsx` is the chrome. Nothing in Rust interprets what is typed or printed, and nothing flows between the terminal and the agent (see Security).

## Data flow

```
xterm onData ──► OMP_BRIDGE.terminalWrite ──► terminal_write ──► bounded queue ──► writer thread ──► pty master ──► shell
xterm.write  ◄── onTerminalData {instance, seq, bytes} ◄── terminal://data/{id} ◄── pump ◄── reader thread ◄── pty master ◄── shell
                  │ after xterm parsed it                                           │ ▲
                  └──► terminal_ack ──────────────────────────────────────────────►┘ │ (stalls at HIGH_WATER unacked)
                                                                         output ring ┘ ◄── terminal_replay
```

Per shell, `pty.rs` runs four threads: **reader** (blocking `read` of the master in 8 KiB chunks), **waiter** (Unix: `waitid(WNOWAIT)`, which observes the exit without reaping; Windows: `child.wait()`), **writer** (drains the input queue into the master) and **pump** (`pump.rs`: coalesces what reader and waiter send, numbers it, appends it to the output ring and emits the events). `ring.rs` is the per-shell output ring, `settings.rs` the `terminal.json` store, `kill.rs` the tree kill.

## Tab registry

`TerminalManager` (`mod.rs`, managed state) holds one entry per **tab** (session id), not per shell:

```rust
struct Tab { folder: Option<PathBuf>, slot: Slot }
enum Slot { Idle, Spawning { instance: u64, shell: PathBuf }, Live(Arc<Pty>) }
```

- **Registration.** `start_session` calls `register_tab(id, folder)` **before** it spawns omp, so a tab whose omp failed to start still has a usable terminal. Re-registering an existing id (a profile switch re-runs `start_session` under the same id) updates the folder and keeps the slot, so the tab keeps its shell.
- **Unknown ids are refused.** Every `terminal_*` command for an id that is not a registered tab fails (`no tab {id}`) or, for `terminal_status`/`terminal_replay`, returns `null`. Only `start_session` creates a tab and only `close_tab`/`close_all_tabs`/`shutdown_all` remove one.
- **Instances.** Every spawn gets an `instance` number from one manager-wide counter starting at 1, so a respawn in the same tab always has a higher instance than any shell before it. Events and replies carry it; the webview drops anything from an older instance.
- **No lifecycle lock.** The map lock is held only to look up, insert, replace or remove an entry, never across a spawn or a kill. `open`:
  1. validate the size, look the tab up (unknown ⇒ error);
  2. a `Spawning` slot or a running `Live` shell ⇒ return its status, no second spawn (overlapping opens get the same instance);
  3. resolve the shell from the cached setting (`pty::resolve_shell`) with no lock held;
  4. under the lock re-check and set `Spawning { instance, shell }`;
  5. spawn with no lock held;
  6. re-lock: if the tab still exists and its slot is still this `Spawning { instance }`, store `Live(pty)`; otherwise the tab was closed (or replaced) meanwhile: unlock, close the fresh shell (see Kill model) and return `tab closed`. A spawn error resets the slot to `Idle` if it is still ours and returns the error.

  An exited `Live` shell is replaced by a new spawn; the old `Pty` is closed after unlocking (its leftovers, see Kill model). `close_tab` during a pending spawn just removes the tab; the opener's re-check in step 6 closes what it spawned, so no shell is orphaned.

## Working directory and shell: Rust's

The webview never sends a path. The shell starts in the tab's registered folder if it is an absolute path to an existing directory, else in the home folder (`$HOME` / `%USERPROFILE%`, `widgets::config::home_dir`), else `open` fails (`no folder for the terminal: the home folder is not set`).

`pty::resolve_shell`, first hit wins:

1. The shell set under Tweaks → Terminal, stored in `<app config>/terminal.json` (see Settings). It is an absolute path to an executable file; since it was set on purpose, a shell that has become unusable makes `open` fail instead of falling through.
2. `pty::default_shell()`, which Tweaks shows as the placeholder: `portable-pty`'s own choice (`CommandBuilder::new_default_prog().get_shell()`), resolved in-process without spawning anything:
   - unix: `$SHELL` if it is executable, else the account's login shell from the user database (`getpwuid`), else `/bin/sh`;
   - Windows: `%ComSpec%`, else `cmd.exe`.

The user database matters because the app's environment may lack `$SHELL` (a launch from a script, some GUI launch paths). Falling straight to `/bin/sh` there starts a different shell than the user's, and a `~/.profile` that launches the real shell would stack two shells, so `exit` would land in the outer one. `getpwuid` may still query a directory service, so `terminal_settings` runs off the IPC thread.

`-l -i` is passed only to sh-family shells (file name `sh`, `bash`, `zsh`, `fish`, `dash`, `ksh`); any other shell gets no arguments.

**Environment.** The app's own, plus `TERM=xterm-256color`, `COLORTERM=truecolor`, `OMP_DESKTOP=1`, `OMP_DESKTOP_SESSION=<id>` and the extended `PATH` from `child_path::child_path()` (the same one the omp child gets; the MSI relaunch has a machine-only PATH). On macOS, when none of `LANG`, `LC_ALL`, `LC_CTYPE` is set (or all are empty) in the app's environment, the shell gets `LANG=en_US.UTF-8`: an app launched from Finder or the Dock has no locale (Terminal.app and iTerm2 set one themselves), and a shell in the C locale mangles non-ASCII file names and input. The value is fixed rather than derived from the system region (`defaults read -g AppleLocale` is a subprocess and need not name an installed locale); a variable the user did set is never overridden.

## Settings (`terminal.json`)

`<app config>/terminal.json` is `{ "shell": "" }` (`""` = the default shell), read once at startup and written through `json_store` like the keybinding overlay (atomic write under the store lock). A missing file is the default. An unreadable or malformed one reads as the default (logged), and `terminal_set_shell` then refuses (`<path> is unreadable: …`) rather than overwrite what the user stored there. Without an app config folder the setting lives in memory only; there is deliberately no temp-dir fallback (as for `widgets.json`), since another account could plant the shell there. `terminal_set_shell` trims, validates (`""`, or `pty::validate_shell`: an absolute path to an executable file), persists, and updates the cached value; `open` reads only the cache, so no disk I/O runs under any lock. The shell applies to the next spawn; running shells are not touched. Nothing is kept in `localStorage`.

## Commands and events

All args are camelCase.

|Command|Args|Returns|Notes|
|---|---|---|---|
|`terminal_open`|`sessionId, cols, rows`|`TerminalStatus`|`async` + `spawn_blocking`. Spawns unless a shell is starting or running (then returns its status). Errors: `invalid terminal size CxR`, `no tab {id}`, no usable folder, unusable shell, spawn failure, `tab closed`.|
|`terminal_status`|`sessionId`|`TerminalStatus \| null`|Sync. `null`: unknown tab or never spawned.|
|`terminal_replay`|`sessionId, instance, afterSeq`|`ReplayWire \| null`|Sync. The current shell's held output with `seq > afterSeq`; resets the flow-control debt. A stale `instance` is answered for the current instance with `afterSeq` taken as 0. `null`: no shell.|
|`terminal_ack`|`sessionId, instance, bytes`|—|Sync. Pays off flow-control debt; ignored for another instance.|
|`terminal_write`|`sessionId, data`|—|Sync. `data` is xterm's `onData` string, written verbatim as UTF-8. Errors: `no tab {id}`, `no terminal for tab {id}` (no shell yet), `terminal input backlog`.|
|`terminal_resize`|`sessionId, cols, rows`|—|Sync. One ioctl on the master. Errors as for write, plus the size check.|
|`terminal_settings`|—|`TerminalSettings`|`async` + blocking: `defaultShell` may query the user database (`getpwuid`).|
|`terminal_set_shell`|`shell`|`TerminalSettings`|`async` + blocking store write. Validates and persists.|
|`close_tab`|`sessionId`|—|`async` + `spawn_blocking`. Ends a tab: stops its omp and session grants (`AgentBridge::stop_session`), tells the widget host (`session_closed`), stops its git watcher, removes its terminal tab and closes the shell (`TerminalManager::close_tab`: hang-up, up to 200 ms grace, tree kill; see Kill model). Unknown ids are fine.|
|`close_all_tabs`|—|—|`async` + `spawn_blocking`. `close_tab` for every id in `AgentBridge::session_ids()` ∪ `TerminalManager::tab_ids()`, so it ends omp processes as well as shells. Called once at webview boot.|

```text
TerminalStatus   { instance, shell, state: "starting" | "running" | "exited", exitCode: number | null,
                   windowsPty: { backend: "conpty", buildNumber: number } | null }
ReplayWire       { instance, b64, headSeq, dropped }
TerminalSettings { shell, defaultShell }     // shell "" = default; defaultShell = what "" resolves to
```

`shell` in `TerminalStatus` is the program path actually running (the panel head shows its file name). `exitCode` is `null` while running and for a signal death. `windowsPty` is `null` off Windows; on Windows it carries the OS build (`HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\CurrentBuildNumber`, read once and cached, `null` if unreadable) for xterm's `windowsPty` option (see Per-tab xterm instances).

|Event|Payload|
|---|---|
|`terminal://data/{id}`|`{ instance, seq, b64 }`: base64 of the pty bytes|
|`terminal://exit/{id}`|`{ instance, code }`: `code` `null` = killed by a signal|

`stop_session` (profile switch) does **not** touch the terminal, and there is no `terminal_close`: `close_tab` is the only way a tab ends.

### Bridge (`live.js`)

IPC glue only, no lifecycle policy:

- `terminalOpen(sid, cols, rows)` → `{ ok: true, status } | { ok: false, error }`; `terminalStatus(sid)` → `status | null`.
- `terminalReplay(sid, instance, afterSeq)` → `{ instance, bytes: Uint8Array, headSeq, dropped } | null`; `terminalAck(sid, instance, n)`; `terminalWrite`, `terminalResize`.
- `onTerminalData(sid, cb({ instance, seq, bytes }))`, `onTerminalExit(sid, cb({ instance, code }))`: each returns `{ stop, ready }`; `ready` resolves once the Tauri listener is registered.
- `terminalSettings()` → `{ shell, defaultShell } | null`; `setTerminalShell(shell)` (trimmed) → `{ ok: true, settings } | { ok: false, error }`.
- `_closeTab(id)` invokes `close_tab` and drops the tab's JS-side bindings; every path that removes a tab from `sessionRegistry` goes through it: `closeSession`, the rollback of a tab that failed to start (`start_session` already registered its terminal), and a profile respawn that finds its tab closed meanwhile. A profile switch itself uses `stop_session`. Boot invokes `close_all_tabs` (after loading profiles) before `_restoreOpenTabs`.

## Lifecycle

- **Open.** The first time a tab's panel is open *and* its xterm is mounted (`needsSpawn`), the hook fits the xterm and calls `terminal_open` with the fitted size, so the pty starts at the right size. A spawn failure is written into the xterm and treated as an exited shell (Enter retries).
- **Hide.** The ✕ in the panel head, the toggle chord, the status-bar button and `/terminal` only change visibility (`open`); the shell keeps running.
- **Exit.** `terminal://exit` of the current instance marks the status `exited` and writes "shell exited (code N) — press Enter to restart" into the xterm. The state stays `exited` across hide/show; Enter (intercepted in xterm's key handler) resets the xterm and calls `terminal_open` again, which spawns a higher instance.
- **Profile switch.** `stop_session` + `start_session` under the same id: omp restarts, `register_tab` updates the folder, the shell keeps running.
- **Close tab.** `closeSession` → `close_tab`; the hook drops that tab's state and disposes its xterm.
- **Webview reload.** The webview's state is gone, but Rust still holds the previous document's tabs, their omp processes and their shells. Tab ids are never reused across a reload (`session-<ms>`), so anything registered at boot belongs to the previous document: live.js invokes `close_all_tabs` before restoring tabs, which ends those omp processes and shells instead of orphaning them. The restored tabs get fresh shells.
- **App exit and relaunch after an update.** Both skip `Drop` and go through `lib.rs::kill_child_processes`, which calls `TerminalManager::shutdown_all` next to `AgentBridge::shutdown_all` and the widget kill: the relaunch ends in `process::exit`, and the shells (session leaders, not children in our process group) would be orphaned. `shutdown_all` removes every tab, hangs every shell up, waits **one** shared grace (200 ms at most, however many tabs) and then kills every shell tree; the manager stays usable (an opener still in flight finds its tab gone and closes its shell).

## Kill model

A shell's whole tree is killed on `close_tab`, replace, `shutdown_all` and shell exit.

- **Hang-up first (closing).** `close_tab`, replacing an exited shell, an opener that lost its tab, and `shutdown_all` do what closing a terminal window does before they kill: `Pty::close` silences the stream (no exit event follows), sends `SIGHUP` to the leader's process group (`kill(-sid, SIGHUP)`, `TreeKiller::hang_up`), waits up to `HANGUP_GRACE` (200 ms) for the leader to exit (it polls the exit the waiter records), then runs the `SIGKILL` passes below. A shell gets to save its history and hang up its own jobs (bash at a prompt does exactly that); one that ignores `SIGHUP` costs the full grace. `shutdown_all` hangs every shell up first and waits for all of them against one deadline, so it costs one grace, not one per tab. Signalling by `-sid` is safe for the reason the kill is: while the session id is stored the leader is an unreaped zombie or alive, so no other process group can carry that id. The waiter's cleanup after a shell exited on its own stays `SIGKILL`-only, and a dropped `Pty` (`Drop`) only kills. On Windows there is no hang-up: the job close below ends the tree at once.
- **Unix: by pty session id.** `portable-pty` makes the shell a session leader (`setsid` before exec), so every process started from the terminal, including background jobs that outlived the shell and were reparented to init, has `getsid(pid) == shell pid`, unless it calls `setsid` itself. Walking the process tree from the shell would miss exactly those reparented jobs. `kill.rs` lists pids (`/proc` on Linux, one `ps` run elsewhere) and `SIGKILL`s every one in the session, repeating (up to 8 passes) until a pass finds none it has not signalled, since a job may fork between listing and signalling. This blocks a few milliseconds, which is why `close_tab`, `open` (when replacing) and `shutdown_all` run off the IPC thread.
- **At shell exit: the leader is held as a zombie.** The session id is the leader's pid, and a pid the system may reissue after the reap is no safe key for a `SIGKILL`: during the 250 ms grace or at a later close or respawn it could belong to an unrelated session. So the waiter never reaps to learn of the exit: `kill::wait_exit` calls `waitid(P_PID, pid, WEXITED | WNOWAIT)` (retrying on `EINTR`), which returns at the exit but leaves the shell an unreaped zombie, and takes the code from `siginfo` (`CLD_EXITED` gives `si_status`, `CLD_KILLED`/`CLD_DUMPED` give none). While the zombie exists no other process can get that pid, so the session id stays valid; a zombie ignores signals, so a `kill` by it reaches only the session's members. The waiter stores the exit code (read by `Pty::exit_code`, hence `terminal_status`), sends the exit to the pump, sleeps the 250 ms grace (the reader's, so a job's last output is still delivered), calls `TreeKiller::kill` (members only; background jobs of an exited shell die with it) and only then reaps with `child.wait()`. `kill` holds the id's lock for its whole run and clears it, so a racing close or respawn either does the kill itself or waits for the one in progress, and signals nothing afterwards; whichever it is, the waiter alone reaps, and only after `kill` has returned, so no kill ever runs against a reissued pid. If `waitid` fails (the child is not ours, or already reaped) the shell is reaped through `child.wait()` and the session id is forgotten unsignalled. The zombie is visible in `ps` (`Z`) for about the grace period after each shell exit. Tests in `terminal/tests.rs` cover the background job dying with its exited shell, an exited shell never being signalled again by close or respawn, the zombie staying until cleanup, a close during the grace period, and the exit code being read without reaping.
- **Windows: Job Object.** The shell is assigned to a terminal-owned Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (the pattern of `agent/supervisor.rs`), so closing the handle kills the shell and all descendants. If the job cannot be set up it falls back to killing the shell alone. `terminal/tests_windows.rs` (run on the Windows CI leg) spawns `cmd.exe` through the manager and checks the exit code, replay, the `windowsPty` status, and that a `start /b ping` job is gone after `close_tab`.
- **ConPTY drain.** The reader keeps reading the master until EOF even after the pump is gone (closed, or done waiting for it); it just stops forwarding. On Windows builds before 11 24H2, `ClosePseudoConsole` (run when the `Pty`, holding the master, is dropped) blocks until the console's output pipe is drained, so a reader that stopped early could hang the close.

`ProcessSupervisor` is not used: `portable-pty` spawns through its own `CommandBuilder`, not `std::process::Command`.

## Output pipeline

- Reader → pump through a queue of 64 chunks (8 KiB each); a full queue blocks the reader, which backpressures the shell (`yes`, `cat bigfile`).
- The pump emits **base64 frames** (bytes are not valid UTF-8 in general, Tauri payloads are JSON), **at least 4 ms apart** (the first after a quiet spell goes out at once) and **at most 64 KiB each**. A larger backlog goes out piecewise, one piece per interval, and the pump stops draining the queue while a full frame waits.
- **seq.** Every data event of a shell carries `seq` = 1, 2, 3, … (per instance; a respawn starts again at 1).
- **Output ring.** Every frame is appended to the shell's `OutputRing` (`ring.rs`, `RING_BYTES` = 1 MiB, whole frames evicted oldest first; a single frame larger than the budget is kept alone, so the newest output is never the one lost) **before** its event goes out, so a replay racing the event already covers it (the webview then drops the event by `seq`). The ring sits in the pump/`Pty` shared `Stream` behind a mutex held only to append or copy. `replay(afterSeq)` returns the bytes of every held frame with `seq > afterSeq`, `headSeq` = the newest seq (0 if none) and `dropped` = some frame after `afterSeq` was already evicted. This is how a webview that missed events (an xterm created after the shell started, a listener registered late, a gap in `seq`) catches up without Rust ever re-emitting.
- **Flow control.** The pump counts unacked bytes (emitted − acked, saturating). At `HIGH_WATER` (512 KiB) unacked it stops emitting **and** stops draining its queue, so the reader blocks and the pty slows the shell, until `terminal_ack` brings the debt strictly below `LOW_WATER` (128 KiB) or `terminal_replay` resets it to 0. It waits on a condition variable that `ack`, `replay` and `kill` notify: no polling, and a killed shell's pump wakes and exits at once even while stalled. The webview acks a frame once xterm has parsed it, so a busy renderer slows the shell instead of piling up events in the IPC queue. Unacked bytes never exceed `HIGH_WATER` + `MAX_EVENT` (64 KiB).
- **Exit event.** Emitted after all data: once the shell has exited and the reader has finished (EOF) with everything flushed, or `READER_GRACE` (250 ms) after the exit if the reader has not reached EOF by then (the late output is dropped). Time spent stalled by flow control does not count against the grace: it restarts after a stall. On macOS the pty hangs up when its session leader exits, so a background job's late output is dropped and the exit follows at once; on Linux a job holding the slave open still delivers output within the grace, before the exit event. Both are covered by tests in `terminal/tests.rs`.
- A killed (closed, replaced, shut down) shell emits nothing further, not even the exit event (`Stream::close` sets the `closed` flag the pump checks).

## Input

Per-shell **writer thread** fed by a **bounded channel** (1024 writes). `terminal_write` only enqueues (`try_send`, never blocks; a full queue returns "terminal input backlog"), the writer does the possibly blocking write to the master in order, and stops if a write fails (shell gone). `terminal_write` and `terminal_resize` stay **synchronous commands on purpose**: Tauri runs sync commands in invocation order, which keystrokes and "the last size wins" need; concurrent async tasks or `spawn_blocking` jobs could reorder them. Hence nothing blocking may run in either.

## Frontend

### Panel state (`app/terminal.js`)

Pure, `window.OMP_TERMINAL`, covered by `test-terminal.mjs`. State `{ [sid]: { open, created, status, error } }`, where `status` is Rust's `TerminalStatus` or `null` and `error` the last `terminal_open` failure or `null`:

- `openPanel`, `closePanel`, `togglePanel`, `prune`, `entryOf`.
- `applyStatus(s, sid, status)`: ignored for `null`, a tab with no entry, an older instance than the one shown, and an earlier state of the same instance (state only moves forward, starting → running → exited), so a late `terminal_open` reply never hides a newer shell or an exit. Clears `error`.
- `applyExit(s, sid, instance, code)`: applied only for the instance shown.
- `spawnFailed(s, sid, error)`: a failed start, shown like an exited shell.
- `needsSpawn(entry)` (`open && created && status === null && error === null`, so a failed start is not retried in a loop), `isExited(entry)` (`error !== null` or state `exited`).
- `keymapShouldBypass(e, isMac?)`, `clipboardAction(e, isMac?)` (see Key handling), `shellLabel`, `clampHeight`.

### Sync (`app/use-terminal.jsx`)

Per tab a record `{ term, fit, instance, lastSeq, syncing, pending, … }`. Replays run one after another on a per-tab promise chain; data and exit events that arrive while one is in flight wait in `pending` and run in arrival order afterwards.

- **Create.** Subscribe to data and exit, await both `ready`, then `terminalStatus` → `applyStatus`; if a shell exists, `terminalReplay(sid, instance, 0)` and write it. Since the listeners are live before the status is read, nothing falls between the replay and the first event (`seq <= lastSeq` is dropped).
- **Data.** Lower instances and `seq <= lastSeq` are ignored. A higher `instance` is adopted (`lastSeq = 0`; the xterm was already reset by the respawn) and the status is fetched again. A gap (`seq > lastSeq + 1`) runs `terminalReplay(sid, instance, lastSeq)` and re-queues the event, which by then is covered by the replay or is written as is. A replay with `dropped` after `afterSeq > 0` writes a dim `[output skipped]` line. Live frames are written with `term.write(bytes, () => terminalAck(sid, instance, bytes.length))`; replayed bytes are not acked (the replay already reset the debt).
- **Exit.** An exit of the instance shown goes through `applyExit` and writes the exit line once; an exit of an unknown newer instance first replays that instance's output, then applies.
- **Spawn.** `terminalOpen` → `applyStatus`, adopt a higher instance, then a replay from `lastSeq` to pick up output emitted before the reply arrived. A failure is written into the xterm and recorded with `spawnFailed`.
- View policy stays in the hook: auto-open, visibility, height, focus, restart on Enter. The hook also backs the Tweaks shell field (`shellSettings`, `shellError`, `saveShell`).

### Per-tab xterm instances

One xterm `Terminal` per tab lives in a ref map in `use-terminal.jsx`, created the first time that tab's panel node mounts. `app-live.jsx` renders a `TerminalPanel` for every tab that has ever opened its panel (`created`, also while its first spawn is pending or has failed) and marks the inactive or collapsed ones `hidden` (they stay mounted until the tab goes), so a hidden tab's instance keeps receiving and acking `terminal://data` and the output is there on return. Un-hiding refits (`FitAddon`) and repaints (`refresh`), since output written while hidden is parsed but not painted. Memory is bounded by the 5 000-line scrollback; renderer is the default DOM one. A closed tab has its listeners stopped, its observer disconnected and its xterm disposed. A `ResizeObserver` on every panel body refits (debounced 50 ms) after any size change of the body: window, sidebar, tree, subagent pane; `onResize` forwards the new size to `terminal_resize`. The inset around the text is padding on xterm's own `.xterm` element, not on `.term-body`: `FitAddon` measures the parent and subtracts the element's padding, so padding on the parent would make it propose rows and columns that are then clipped. Right-click on a selection copies it (`copySelection`); without a selection the default applies.

**ConPTY.** Every status from Rust (create, spawn, refetch) goes through `noteStatus`, which also sets `term.options.windowsPty` from `status.windowsPty` (`{ backend: "conpty", buildNumber }`, the Windows build Rust read from the registry; `null` elsewhere, which leaves the option unset). xterm then applies its ConPTY wrapping heuristics on builds that need them (< 21376). The option is written only when it differs from the current one.

The panel head shows `shellLabel(status.shell)`, the shell actually running. Tweaks → Terminal's shell field reads `terminalSettings()` (placeholder `default (<defaultShell>)`) and saves on blur or Enter through `setTerminalShell`, showing Rust's refusal inline (`.term-shell-error`). Colours follow the active theme's CSS variables; font size is 13 px × the chat font-size tweak. Panel height is the shared tweak `terminalHeight` (default 240 px, clamped to 120 px … 70 % of the session column by `clampHeight`); open/closed state is per tab and not persisted. Dragging the panel's top edge captures the pointer (`setPointerCapture` on the handle) and resizes the panel from `TerminalPanel`'s local state; the tweak is written once, on `pointerup` / `pointercancel` / lost capture, and only when the height changed — not once per `pointermove` (every tweak write re-renders the app and persists). The body's `ResizeObserver` refits during the drag. `terminalAutoOpen` opens the panel for each new tab.

## Key handling

xterm's focus target is a hidden `<textarea>`, and the global keymap listens on `window` and ignores events with `defaultPrevented`. xterm consumes (and prevents) the keys it handles, so those never reach the keymap; `attachCustomKeyEventHandler` returns `false` for a keydown that `keymapShouldBypass` (`app/terminal.js`) accepts, which hands it back to the page. The decision follows the **resolved keymap**, not a fixed list: the chord is normalised with `OMP_KEYMAP.chordFromEvent` like the keymap itself and looked up with `OMP_KEYMAP.lookup`, so a rebinding in the shortcuts screen applies inside the terminal too. A chord goes to the app iff it resolves to a `global`-scope action and

- the action is in `ALWAYS`: `desktop.terminal.toggle` (the chord that opened the panel also closes it, whatever it is bound to) and `desktop.tab.next` / `desktop.tab.prev`; or
- the chord has the `super` modifier (the macOS ⌘ forms of new/close tab, command bridge, history, sidebar, shortcuts, toggle); or
- off macOS, it has both `ctrl` and `shift` (`ctrl+shift+p`, `ctrl+shift+pageup`, … — no shell binds these).

Everything else stays with the shell, notably:

- Unbound chords. Unbinding the toggle (or rebinding it elsewhere) frees `` ctrl+` `` for the shell.
- Plain `ctrl+<key>` and `alt+<key>`, even when the keymap binds them: `ctrl+t` and `ctrl+w` are readline's transpose and kill-word, `ctrl+k`, `ctrl+h`, `ctrl+b`, `ctrl+/`, `ctrl+p` are kill-line, backspace, back-char, undo, history. The `super+` forms cover the tab actions on macOS; elsewhere use the tab bar or hide the panel first. (The original spec listed them as tab actions; this is the ruling that replaced it.)
- Composer-scope actions (`app.message.followUp`): they only act in the prompt box.
- `ctrl+c` is the shell's interrupt, and `Esc` belongs to the shell and TUIs (vim). xterm consumes `Esc`, so it does **not** interrupt the agent turn while the terminal has focus; click out of the terminal first.
- The platform's copy / paste chords, even when bound: `ctrl+shift+c` / `ctrl+shift+v` off macOS, `super+c` / `super+v` on macOS.

**Copy / paste.** On Linux and Windows the key handler handles `ctrl+shift+c` / `ctrl+shift+v` itself (`clipboardAction`): copy puts xterm's selection on the clipboard through `window.copyText` (nothing happens without a selection), paste reads `navigator.clipboard.readText()` and hands the text to `term.paste` (bracketed paste when the program asked for it). Both call `preventDefault` and return `false`, so neither the keymap nor the WebView's own paste sees the event. On macOS ⌘C / ⌘V stay native (xterm and the WebView). Right-click on a selection copies it everywhere (`copySelection`).

The platform comes from `OMP_KEYMAP.detectMac()`, read once at load; both functions take an optional `isMac` so `test-terminal.mjs` checks both platforms (rebound and unbound toggle, super, ctrl+shift, plain ctrl/alt, composer scope, copy/paste chords).

## Vendored xterm

`@xterm/xterm` and `@xterm/addon-fit` are devDependencies copied verbatim (UMD builds, minus the trailing sourcemap comment) into `src/xterm.js`, `src/xterm.css`, `src/xterm-addon-fit.js` by `npm run vendor:xterm` (`scripts/vendor-xterm.mjs`); never edit them by hand. They expose `window.Terminal` and `window.FitAddon.FitAddon`. xterm needs no `eval` or inline script, so the release CSP is unchanged.

## Security: your terminal, not the agent's

The shell runs with the user's full rights. omp's approval modes and the app's approval rules do **not** apply to anything typed there; this is by design. Nothing flows between terminal and agent: the agent cannot read or drive the terminal, and the terminal's output is not added to the prompt.

The trust boundary is the integrity of the webview's JavaScript, as for tool approvals and rail widgets: code running in the webview can type into any tab's shell, as it can approve a tool call. What Rust does keep out of the webview's hands: it accepts only ids of registered tabs (a terminal exists only for a tab `start_session` registered), it picks the working directory itself (the webview never sends a path), and the one string that selects a program, the shell setting, is validated as an absolute path to an executable file before it is persisted. Output is rendered by xterm into DOM text nodes. **OSC 8 hyperlinks are disabled** (`term.parser.registerOscHandler(8, () => true)` in `create`): xterm 6 enables them by default and keeps an invisible link entry per hyperlink for as long as its cells live, unbounded for a program that streams them, and no links addon is loaded to use them. **OSC 52 (clipboard) and title changes are not wired**: no clipboard addon is loaded and nothing listens to xterm's `onTitleChange`, so program output can neither touch the clipboard nor change the window title.

## Not covered

One shell per tab, no splits; no WebGL renderer; a webview reload ends every shell (`close_all_tabs`) rather than reattaching; runtime verification is macOS and Linux (the unix tests); the Windows tests (`tests_windows.rs`) compile here and run on the Windows CI leg only.
