//! Per-tab shell on a pseudo-terminal (the terminal panel's Rust side).
//!
//! **Tabs, not shells.** `TerminalManager` keeps one entry per tab (session
//! id), registered by `start_session` with the tab's project folder and
//! removed only by `close_tab` / `shutdown_all`. Its [`Slot`] says whether
//! the tab has a shell: none yet, one being spawned, or one (running or
//! exited). The webview sends ids and view state only: Rust picks the
//! working directory (the tab's folder, else the home folder) and the shell
//! (the `terminal.json` setting, see [`settings`]), so no path from the
//! webview reaches a spawn, and an id that is not a registered tab is
//! refused.
//!
//! **Events.** Output goes out as `terminal://data/{id}`
//! `{ instance, seq, b64 }` and, once the shell has exited,
//! `terminal://exit/{id}` `{ instance, code }` (`code` null = killed by a
//! signal). `instance` numbers every shell this manager starts (one counter
//! for all tabs, from 1), `seq` every data event of one instance, so the
//! webview can drop stale events and fill gaps with `replay`.
//!
//! **Locking.** The map is held only to look up, insert, replace or remove
//! an entry — never across a spawn or a kill. `open` marks the slot
//! `Spawning` with a fresh instance, spawns unlocked, and stores the result
//! only if the slot is still that `Spawning`; otherwise (the tab was closed
//! meanwhile) it kills its own fresh shell. Overlapping opens see the
//! `Spawning` slot and return its status instead of spawning again. Nothing
//! that blocks runs on the IPC thread: input is queued for the shell's
//! writer thread, and spawning and killing run in `spawn_blocking`.

mod kill;
mod pty;
mod pump;
mod ring;
mod settings;
mod shell;
mod stream;
#[cfg(test)]
mod test_support;
#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod tests_windows;
mod threads;
mod wire;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;

pub use pump::Emit;
pub use settings::TerminalSettings;
pub use wire::{ReplayWire, TauriEmit, TerminalState, TerminalStatus};

use pty::Pty;
use settings::SettingsStore;
use stream::ShellState;
use wire::windows_pty;

struct Tab {
    /// The project folder `start_session` registered.
    folder: Option<PathBuf>,
    slot: Slot,
}

impl Tab {
    /// The tab's running shell, if it has one.
    fn into_shell(self) -> Option<Arc<Pty>> {
        match self.slot {
            Slot::Live(pty) => Some(pty),
            Slot::Idle | Slot::Spawning { .. } => None,
        }
    }
}

enum Slot {
    /// No shell yet (or its spawn failed).
    Idle,
    /// An `open` is spawning `instance` without holding the map.
    Spawning { instance: u64, shell: PathBuf },
    /// A shell, running or exited.
    Live(Arc<Pty>),
}

impl Slot {
    fn status(&self) -> Option<TerminalStatus> {
        match self {
            Self::Idle => None,
            Self::Spawning { instance, shell } => Some(TerminalStatus {
                instance: *instance,
                shell: shell.display().to_string(),
                state: TerminalState::Starting,
                exit_code: None,
                windows_pty: windows_pty(),
            }),
            Self::Live(pty) => Some(live_status(pty)),
        }
    }

    /// The status when there is nothing for `open` to do: a shell is being
    /// spawned or is running.
    fn current_status(&self) -> Option<TerminalStatus> {
        self.status()
            .filter(|status| status.state != TerminalState::Exited)
    }

    const fn is_spawning(&self, instance: u64) -> bool {
        matches!(self, Self::Spawning { instance: i, .. } if *i == instance)
    }
}

fn live_status(pty: &Pty) -> TerminalStatus {
    let (state, exit_code) = match pty.state() {
        ShellState::Running => (TerminalState::Running, None),
        ShellState::Exited(code) => (TerminalState::Exited, code),
    };
    TerminalStatus {
        instance: pty.instance(),
        shell: pty.shell().display().to_string(),
        state,
        exit_code,
        windows_pty: windows_pty(),
    }
}

/// Managed state: the tabs and their shells.
pub struct TerminalManager {
    /// Held only for lookups, inserts, replacements and removals.
    tabs: Mutex<HashMap<String, Tab>>,
    /// The next shell's instance number (for every tab).
    next_instance: AtomicU64,
    settings: SettingsStore,
}

impl TerminalManager {
    /// A manager whose shell setting lives in `settings_path`
    /// (`terminal.json`; read once here), or in memory only when `None`.
    pub fn new(settings_path: Option<PathBuf>) -> Self {
        Self {
            tabs: Mutex::new(HashMap::new()),
            next_instance: AtomicU64::new(1),
            settings: SettingsStore::load(settings_path),
        }
    }

    /// The map. A panic while it was held cannot leave it inconsistent
    /// (every change is one insert, replace or remove), so poison is ignored.
    fn tabs(&self) -> MutexGuard<'_, HashMap<String, Tab>> {
        self.tabs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Make `id` a tab whose shell starts in `folder`, or update the folder
    /// of an existing one, keeping its shell (a profile switch restarts omp
    /// under the same id; the tab and its terminal stay).
    pub fn register_tab(&self, id: &str, folder: Option<&Path>) {
        let folder = folder.map(Path::to_path_buf);
        let mut tabs = self.tabs();
        if let Some(tab) = tabs.get_mut(id) {
            tab.folder = folder;
        } else {
            tabs.insert(
                id.to_owned(),
                Tab {
                    folder,
                    slot: Slot::Idle,
                },
            );
        }
    }

    /// Every registered tab.
    pub fn tab_ids(&self) -> Vec<String> {
        self.tabs().keys().cloned().collect()
    }

    /// Start the tab's shell and return its status. While one is starting or
    /// running, returns that one's status instead (no second spawn); an
    /// exited one is replaced, and its leftovers are killed. `emit` becomes
    /// the new shell's emitter. Blocks while spawning and killing: call off
    /// the IPC thread.
    pub fn open<E: Emit>(
        &self,
        emit: E,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<TerminalStatus, String> {
        check_size(cols, rows)?;
        #[cfg(windows)]
        wire::require_conpty(windows_pty())?;
        let folder = {
            let tabs = self.tabs();
            let tab = tabs.get(id).ok_or_else(|| no_tab(id))?;
            if let Some(status) = tab.slot.current_status() {
                return Ok(status);
            }
            let folder = tab.folder.clone();
            drop(tabs);
            folder
        };
        // Both may touch the disk (and the default shell may query the user
        // database): before taking the map again.
        let preference = self.settings.shell();
        let (program, shell_args) = shell::resolve_shell(&preference)?;
        let cwd = tab_cwd(folder.as_deref())?;

        let (instance, replaced) = {
            let mut tabs = self.tabs();
            let tab = tabs.get_mut(id).ok_or_else(|| no_tab(id))?;
            if let Some(status) = tab.slot.current_status() {
                return Ok(status);
            }
            let instance = self.next_instance.fetch_add(1, Ordering::Relaxed);
            // The slot keeps its own copy for `status` while `spawn` below
            // moves `program` into the `Pty`.
            let starting = Slot::Spawning {
                instance,
                shell: program.clone(),
            };
            let replaced = std::mem::replace(&mut tab.slot, starting);
            drop(tabs);
            (instance, replaced)
        };
        // The exited shell's leftovers, hung up and killed outside the lock.
        if let Slot::Live(old) = replaced {
            old.close();
        }

        let spawned = pty::spawn(
            emit.terminal(id, instance),
            instance,
            pty::SpawnSpec {
                session_id: id,
                cwd: &cwd,
                program,
                shell_args: &shell_args,
                cols,
                rows,
            },
        );
        let orphan = {
            let mut tabs = self.tabs();
            match tabs.get_mut(id) {
                Some(tab) if tab.slot.is_spawning(instance) => {
                    return match spawned {
                        Ok(pty) => {
                            let pty = Arc::new(pty);
                            let status = live_status(&pty);
                            tab.slot = Slot::Live(pty);
                            Ok(status)
                        }
                        Err(e) => {
                            tab.slot = Slot::Idle;
                            Err(e)
                        }
                    };
                }
                // Closed (or shut down) while spawning: the shell is ours
                // alone to kill.
                _ => spawned?,
            }
        };
        orphan.close();
        Err("tab closed".to_owned())
    }

    /// The tab's shell, `None` for an unknown tab or one without a shell.
    pub fn status(&self, id: &str) -> Option<TerminalStatus> {
        self.tabs().get(id).and_then(|tab| tab.slot.status())
    }

    /// The tab's shell (running or exited), outside the lock.
    fn live(&self, id: &str) -> Result<Arc<Pty>, String> {
        let tabs = self.tabs();
        match tabs.get(id).map(|tab| &tab.slot) {
            Some(Slot::Live(pty)) => Ok(Arc::clone(pty)),
            Some(_) => Err(format!("no terminal for tab {id}")),
            None => Err(no_tab(id)),
        }
    }

    /// Queue `data` for the shell, exactly as given; the terminal's writer
    /// thread writes it, in order. Never blocks, which is why the command can
    /// stay synchronous: concurrent async tasks could reorder keystrokes.
    pub fn write(&self, id: &str, data: impl Into<Vec<u8>>) -> Result<(), String> {
        self.live(id)?.write(data.into())
    }

    /// Tell the shell its window is now `cols` × `rows` characters (one quick
    /// ioctl, so synchronous too, for the same ordering reason).
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<(), String> {
        check_size(cols, rows)?;
        self.live(id)?.resize(cols, rows)
    }

    /// The output the tab's shell still holds after event `after_seq`. An
    /// `instance` that is not the current shell's asks for everything the
    /// current one holds. `None` while the tab has no shell. Also lets a
    /// pump stalled on unacknowledged output resume.
    pub fn replay(&self, id: &str, instance: u64, after_seq: u64) -> Option<ReplayWire> {
        let pty = self.live(id).ok()?;
        let current = pty.instance();
        let after_seq = if instance == current { after_seq } else { 0 };
        let replay = pty.replay(after_seq);
        Some(ReplayWire {
            instance: current,
            b64: STANDARD.encode(&replay.bytes),
            head_seq: replay.head_seq,
            dropped: replay.dropped,
        })
    }

    /// The webview has rendered `bytes` more of `instance`'s output. Ignored
    /// for any other instance (an old shell's late acknowledgement).
    pub fn ack(&self, id: &str, instance: u64, bytes: u64) {
        if let Ok(pty) = self.live(id) {
            if pty.instance() == instance {
                pty.ack(bytes);
            }
        }
    }

    /// End the tab: forget it, hang its shell up, and kill what is left of
    /// it and everything it started after [`pty::HANGUP_GRACE`] at most
    /// (see [`Pty::close`]). Silent (no exit event). Unknown tabs are
    /// ignored. An `open` spawning for it meanwhile closes its own shell.
    /// Blocks up to the grace: call off the IPC thread.
    pub fn close_tab(&self, id: &str) {
        self.close_tabs(&[id]);
    }

    /// [`TerminalManager::close_tab`] for exactly these tabs; a tab
    /// registered meanwhile is left alone. Their shells share one
    /// [`pty::HANGUP_GRACE`] and each kill pass over the process table (see
    /// [`Pty::close_all`]).
    pub fn close_tabs<S: AsRef<str>>(&self, ids: &[S]) {
        let shells: Vec<Arc<Pty>> = {
            let mut tabs = self.tabs();
            ids.iter()
                .filter_map(|id| tabs.remove(id.as_ref()))
                .filter_map(Tab::into_shell)
                .collect()
        };
        close_shells(&shells);
    }

    /// End every tab, synchronously — for the paths where the process ends
    /// without running destructors (app exit, an update's relaunch). Like
    /// [`TerminalManager::close_tabs`], so exiting takes one grace, not one
    /// per tab. The manager stays usable.
    pub fn shutdown_all(&self) {
        let shells: Vec<Arc<Pty>> = self
            .tabs()
            .drain()
            .filter_map(|(_, tab)| tab.into_shell())
            .collect();
        close_shells(&shells);
    }

    /// The shell setting and its default. The default may query the user
    /// database (`getpwuid`, possibly a directory service): call off the IPC
    /// thread.
    pub fn settings(&self) -> TerminalSettings {
        self.settings.settings()
    }

    /// Validate and store the shell setting (empty = default); used by the
    /// next shell started. Blocking file I/O.
    pub fn set_shell(&self, shell: &str) -> Result<TerminalSettings, String> {
        self.settings.set_shell(shell)
    }
}

/// Close the shells of tabs already taken out of the map.
fn close_shells(shells: &[Arc<Pty>]) {
    let shells: Vec<&Pty> = shells.iter().map(|pty| &**pty).collect();
    Pty::close_all(&shells);
}

fn check_size(cols: u16, rows: u16) -> Result<(), String> {
    if cols == 0 || rows == 0 {
        return Err(format!("invalid terminal size {cols}x{rows}"));
    }
    Ok(())
}

/// Where a tab's shell starts: its folder when that is an existing
/// directory, else the home folder.
fn tab_cwd(folder: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(dir) = folder.filter(|dir| dir.is_absolute() && dir.is_dir()) {
        return Ok(dir.to_path_buf());
    }
    crate::widgets::config::home_dir()
        .map(PathBuf::from)
        .filter(|home| home.is_absolute() && home.is_dir())
        .ok_or_else(|| "no folder for the terminal: the home folder is not set".to_owned())
}

fn no_tab(id: &str) -> String {
    format!("no tab {id}")
}
