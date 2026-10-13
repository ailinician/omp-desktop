# Terminal panel

A real shell per tab in a panel above the composer, in the tab's project folder. Rust (`src-tauri/src/terminal/`) owns the pty and the process tree; `src/app/terminal.js` (`window.OMP_TERMINAL`, regression `test-terminal.mjs`) decides panel state and key routing; `src/app/use-terminal.jsx` owns one xterm.js instance per tab; `src/design/terminal-panel.jsx` is the chrome. Nothing in Rust interprets what is typed or printed, and nothing flows between the terminal and the agent (see Security).

## Data flow

```
xterm onData ──► OMP_BRIDGE.terminalWrite ──► terminal_write ──► bounded queue ──► writer thread ──► pty master ──► shell
xterm.write  ◄── onTerminalData (base64 → bytes) ◄── terminal://data/{id} ◄── pump ◄── reader thread ◄── pty master ◄── shell
```

Per terminal, `pty.rs` runs four threads: **reader** (blocking `read` of the master in 8 KiB chunks), **waiter** (Unix: `waitid(WNOWAIT)`, which observes the exit without reaping; Windows: `child.wait()`), **writer** (drains the input queue into the master) and **pump** (`pump.rs`: coalesces what reader and waiter send and emits the events). `TerminalManager` (`mod.rs`, managed state) holds `HashMap<session_id, Arc<Pty>>`; the map lock is only held to look up, insert or remove an entry.

## Commands and events

|Command|Args|Notes|
|---|---|---|
|`terminal_open`|`{ args: { sessionId, cwd, shell, cols, rows } }`|`async` + `spawn_blocking`. No-op while the session's shell is alive; otherwise spawns, and a previous (exited) entry is replaced and its leftovers killed. Errors: empty id, zero size, `cwd` not a directory, an unusable `shell`.|
|`terminal_write`|`sessionId, data`|Sync. `data` is xterm's `onData` string, written verbatim as UTF-8.|
|`terminal_resize`|`sessionId, cols, rows`|Sync. One ioctl on the master. Zero size is an error.|
|`terminal_close`|`sessionId`|`async` + `spawn_blocking`. Kills the tree, drops the entry, emits nothing. Unknown ids are fine. `stop_session` does **not** call it: a profile switch stops and respawns omp under the same tab id and the tab keeps its shell.|

|Event|Payload|
|---|---|
|`terminal://data/{id}`|`{ "b64": "<base64 of the pty bytes>" }`|
|`terminal://exit/{id}`|`{ "code": number \| null }` (`null` = killed by a signal)|

`live.js` wraps these as `OMP_BRIDGE.terminalOpen/Write/Resize/Close` and `onTerminalData/onTerminalExit` (the latter two return an unlisten function that is valid at once). `terminalOpen` resolves `cwd` to the tab's project path, else the home folder; if the tab was closed while `terminal_open` was in flight it closes the fresh pty again so no shell is orphaned. `closeSession` calls `terminal_close`; a profile respawn does not, so the tab keeps its shell.

## Shell resolution

`pty::resolve_shell`, first hit wins:

1. The Tweaks → Terminal shell path (`tweaks.terminalShell`, handed to the bridge by `setTerminalShell`). It must be an absolute path to an executable file, or `terminal_open` fails: it was typed on purpose, so it is the only step that errors instead of falling through.
2. `$SHELL`, if it is an executable file.
3. The account's login shell from the user database: `dscl . -read /Users/<user> UserShell` on macOS, `getent passwd <user>` elsewhere on unix (7th field). Looked up only when step 2 yielded nothing, since it spawns a process.
4. `/bin/sh` (`cmd.exe` on Windows, where step 3 does not exist).

Step 3 exists because the app's environment may lack `$SHELL` (a launch from a script, some GUI launch paths). Falling straight to `/bin/sh` there starts a different shell than the user's, and a `~/.profile` that launches the real shell would stack two shells, so `exit` would land in the outer one.

`-l -i` is passed only to sh-family shells (file name `sh`, `bash`, `zsh`, `fish`, `dash`, `ksh`); any other shell gets no arguments. Environment: the app's own, plus `TERM=xterm-256color`, `COLORTERM=truecolor`, `OMP_DESKTOP=1`, `OMP_DESKTOP_SESSION=<id>` and the extended `PATH` from `child_path` (the same one the omp child gets).

## Lifecycle

- **Open.** The first time a tab's panel is open *and* its xterm is mounted, the effect in `use-terminal.jsx` fits the xterm and calls `terminal_open` with the fitted size, so the pty starts at the right size. A spawn failure is written into the xterm and treated as an exited shell (Enter retries).
- **Hide.** The ✕ in the panel head, the toggle chord, the status-bar button and `/terminal` only change visibility (`open`); the shell keeps running.
- **Exit.** `terminal://exit` marks the entry `exited` and writes "shell exited (code N) — press Enter to restart" into the xterm. The state stays `exited` across hide/show; Enter (intercepted in xterm's key handler) resets the xterm and calls `terminal_open` again.
- **Close tab.** `closeSession` → `terminal_close` kills the tree; the hook then drops that tab's state and disposes its xterm. A profile switch (`stop_session`) leaves the terminal alone.
- **App relaunch after an update.** `updater.rs` calls `TerminalManager::shutdown_all` next to `AgentBridge::shutdown_all`: the relaunch ends in `process::exit`, so no destructor would run and the shells (session leaders, not children in our process group) would be orphaned.
- **Serialization.** `open`, `close` and `shutdown_all` run on separate worker threads and all take one manager-wide lifecycle lock for their whole duration (taken before the map lock). Without it, a `close` during a pending `open` could return before the fresh shell is inserted (an orphan), and two overlapping opens would both spawn. One lock rather than one per session: a spawn takes ~10 ms, tabs open and close rarely, and there is no lock table to prune. `write` and `resize` never take it.

## Kill model

A terminal's whole tree is killed on close, replace, shutdown and shell exit.

- **Unix: by pty session id.** `portable-pty` makes the shell a session leader (`setsid` before exec), so every process started from the terminal, including background jobs that outlived the shell and were reparented to init, has `getsid(pid) == shell pid`, unless it calls `setsid` itself. Walking the process tree from the shell would miss exactly those reparented jobs. `kill.rs` lists pids (`/proc` on Linux, one `ps` run elsewhere) and `SIGKILL`s every one in the session, repeating (up to 8 passes) until a pass finds none it has not signalled, since a job may fork between listing and signalling. This blocks a few milliseconds, which is why `close`, `open` (when replacing) and `shutdown_all` run off the IPC thread.
- **At shell exit: the leader is held as a zombie.** The session id is the leader's pid, and a pid the system may reissue after the reap is no safe key for a `SIGKILL`: during the 250 ms grace or at a later `close` or respawn it could belong to an unrelated session. So the waiter never reaps to learn of the exit: `kill::wait_exit` calls `waitid(P_PID, pid, WEXITED | WNOWAIT)` (retrying on `EINTR`), which returns at the exit but leaves the shell an unreaped zombie, and takes the code from `siginfo` (`CLD_EXITED` gives `si_status`, `CLD_KILLED`/`CLD_DUMPED` give none, like a signal death before). While the zombie exists no other process can get that pid, so the session id stays valid; a zombie ignores signals, so a `kill` by it reaches only the session's members. The waiter sends the exit event, sleeps the 250 ms grace (the reader's, so a job's last output is still delivered), calls `TreeKiller::kill` (members only; background jobs of an exited shell die with it) and only then reaps with `child.wait()`. `kill` holds the id's lock for its whole run and clears it, so a racing `close` or respawn either does the kill itself or waits for the one in progress, and signals nothing afterwards; whichever it is, the waiter alone reaps, and only after `kill` has returned, so no kill ever runs against a reissued pid. If `waitid` fails (the child is not ours, or already reaped) the shell is reaped through `child.wait()` and the session id is forgotten unsignalled. The zombie is visible in `ps` (`Z`) for about the grace period after each shell exit. Tests: `a_background_job_dies_with_its_exited_shell_without_any_close`, `an_exited_shell_is_never_signalled_again_by_close_or_respawn`, `the_exited_shell_stays_a_zombie_until_its_session_is_cleaned_up` and `a_close_during_the_grace_period_leaves_the_reaping_to_the_waiter` (a `#[cfg(test)]` hook pauses the waiter before its cleanup and `ps -o stat=` shows `Z`), `the_exit_code_is_read_without_reaping_the_shell`.
- **Windows: Job Object.** The shell is assigned to a terminal-owned Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (the pattern of `agent/supervisor.rs`), so closing the handle kills the shell and all descendants. If the job cannot be set up it falls back to killing the shell alone. **Unverified at runtime.** Only macOS was verified at runtime; the Windows compile (via `portable-pty`) and the Linux runtime are pending CI.

`ProcessSupervisor` is not used: `portable-pty` spawns through its own `CommandBuilder`, not `std::process::Command`.

## Output pipeline

- Reader → pump through a queue of 64 chunks (8 KiB each); a full queue blocks the reader, which backpressures the shell (`yes`, `cat bigfile`).
- The pump emits **base64 frames** (bytes are not valid UTF-8 in general, Tauri payloads are JSON), **at least 4 ms apart** (the first after a quiet spell goes out at once) and **at most 64 KiB each**. A larger backlog goes out piecewise, one piece per interval, and the pump stops draining the queue while a full frame waits.
- **Exit event.** Emitted once the shell is reaped and the reader has finished (EOF) with everything flushed, or 250 ms after the shell exited if the reader has not reached EOF by then (the late output is dropped). On macOS the pty hangs up when its session leader exits, so a background job's late output is dropped and the exit follows at once; on Linux a job holding the slave open still delivers output within the 250 ms grace, before the exit event. Both are covered by tests in `terminal/tests.rs`.
- A closed or replaced terminal emits nothing further (`closed` flag checked by the pump).

## Input

Per-terminal **writer thread** fed by a **bounded channel** (1024 writes). `terminal_write` only enqueues (`try_send`, never blocks; a full queue returns "terminal input backlog"), the writer does the possibly blocking write to the master in order, and stops if a write fails (shell gone). `terminal_write` and `terminal_resize` stay **synchronous commands on purpose**: Tauri runs sync commands in invocation order, which keystrokes and "the last size wins" need; concurrent async tasks or `spawn_blocking` jobs could reorder them. Hence nothing blocking may run in either.

## Key handling

xterm's focus target is a hidden `<textarea>`, and the global keymap listens on `window` and ignores events with `defaultPrevented`. xterm consumes (and prevents) the keys it handles, so those never reach the keymap; `attachCustomKeyEventHandler` returns `false` for the chords in `BYPASS` (`app/terminal.js`, `keymapShouldBypass`, which normalises with `OMP_KEYMAP.chordFromEvent` like the keymap itself), which hands them back to the page:

- `` ctrl+` `` (and `` super+` ``, `super+~`): toggle terminal, so the chord that opened the panel also closes it.
- `super+t`, `super+w`, `super+k`, `super+h`, `super+b`, `super+/`: the macOS ⌘ variants of new/close tab, command bridge, history, sidebar, shortcuts.
- `ctrl+tab`, `ctrl+shift+tab`: next/previous tab (no shell meaning).

Everything else stays with the shell, notably:

- `ctrl+t` and `ctrl+w` are readline's transpose and kill-word; stealing them for new/close tab would make the shell unusable. The `super+` forms cover the same actions on macOS; elsewhere use the tab bar or hide the panel first. (The original spec listed them as tab actions; this is the ruling that replaced it.)
- `ctrl+c` is the shell's interrupt. Copy is `super+c` / `ctrl+shift+c`, paste `super+v` / `ctrl+shift+v` (xterm).
- `Esc` belongs to the shell and TUIs (vim). xterm consumes it, so it does **not** interrupt the agent turn while the terminal has focus; click out of the terminal first.
- `ctrl+k`, `ctrl+h`, `ctrl+b`, `ctrl+/` and the other `ctrl+` chords the keymap binds likewise reach the shell (kill-line, backspace, back-char, undo).

## Per-tab xterm instances

One xterm `Terminal` per tab lives in a ref map in `use-terminal.jsx`, created the first time that tab's panel node mounts. `app-live.jsx` renders a `TerminalPanel` for every tab that has ever opened its panel (`created` in the panel state, also while its first spawn is pending or has failed) and marks the inactive or collapsed ones `hidden` (they stay mounted until the tab goes), so a hidden tab's instance keeps receiving `terminal://data` and the output is there on return. Un-hiding refits (`FitAddon`) and repaints (`refresh`), since output written while hidden is parsed but not painted. Memory is bounded by the 5 000-line scrollback; renderer is the default DOM one. A tab that was closed has its listeners removed, its observer disconnected and its xterm disposed. A `ResizeObserver` on every panel body refits (debounced 50 ms) after any size change of the body: window, sidebar, tree, subagent pane; `onResize` forwards the new size to `terminal_resize`. Right-click on a selection copies it (`copySelection`); without a selection the default applies.

**Spawn generations.** Each shell spawn of a tab has a number. A shell that dies at once can emit `terminal://exit` before `terminal_open` resolves; the exit is recorded with the generation current at that moment (`exitGen`), and `markSpawned` of the same generation keeps it, while the respawn's (next) generation clears it (`terminal.js`, covered by `test-terminal.mjs`).

Colours follow the active theme's CSS variables; font size is 13 px × the chat font-size tweak. Panel height is the shared tweak `terminalHeight` (default 240 px, clamped to 120 px … 70 % of the session column by `clampHeight`); open/closed state is per tab and not persisted. `terminalAutoOpen` opens the panel for each new tab.

## Vendored xterm

`@xterm/xterm` and `@xterm/addon-fit` are devDependencies copied verbatim (UMD builds, minus the trailing sourcemap comment) into `src/xterm.js`, `src/xterm.css`, `src/xterm-addon-fit.js` by `npm run vendor:xterm` (`scripts/vendor-xterm.mjs`); never edit them by hand. They expose `window.Terminal` and `window.FitAddon.FitAddon`. xterm needs no `eval` or inline script, so the release CSP is unchanged.

## Security: your terminal, not the agent's

The shell runs with the user's full rights. omp's approval modes and the app's approval rules do **not** apply to anything typed there; this is by design. In v1 nothing flows between terminal and agent: the agent cannot read or drive the terminal, and the terminal's output is not added to the prompt. The webview sends raw bytes to the pty, so the only trust boundary is the user's own keyboard; the one string that crosses IPC as a command is the shell path chosen in Tweaks, validated as an absolute path to an executable file. Output is rendered by xterm into DOM text nodes. **OSC 8 hyperlinks are disabled** (`term.parser.registerOscHandler(8, () => true)` in `create`): xterm 6 enables them by default and keeps an invisible link entry per hyperlink for as long as its cells live, unbounded for a program that streams them, and no links addon is loaded to use them. **OSC 52 (clipboard) and title changes are not wired**: no clipboard addon is loaded and nothing listens to xterm's `onTitleChange`, so program output can neither touch the clipboard nor change the window title.

## Not covered

One shell per tab, no splits; no WebGL renderer; runtime verification is macOS only, the Windows compile and ConPTY behaviour and the Linux runtime are pending CI (see Kill model).
