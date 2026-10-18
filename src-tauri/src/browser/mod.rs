//! App-managed headless Chromium backing the per-tab browser panel.
//!
//! One [`BrowserManager`] entry per tab: a loopback placeholder port (the
//! `cdpUrl` omp is given through a settings overlay file, see
//! [`BrowserManager::allocate`]) in front of a lazily launched Chromium with
//! its own throwaway profile. Which browser runs is Rust's to decide: the
//! user's `browser.json` setting ([`SettingsStore`]) or automatic discovery,
//! read at launch time. A tab's browser survives its omp process being
//! respawned (profile switch, approval-mode restart); only closing the tab
//! ([`BrowserManager::release_many`]) or leaving the app ends it.

mod annotate;
mod broker;
mod cdp;
pub mod commands;
mod process;
pub mod proxy;
mod view;
mod waiters;
mod wire;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::private_fs::private_dirs;
use broker::Broker;
use process::{prepare_instance_dir, prune_dead, remove_dir_retrying, write_overlay, Chromium};
pub use process::{BrowserSettings, SettingsStore};
use proxy::Proxy;

/// Initial window of every launched Chromium; the panel resizes it later.
const WINDOW: (u32, u32) = (1280, 800);

/// Longest session id accepted; ids become file names.
const MAX_ID_LEN: usize = 64;

/// What a call for a session that was never allocated a browser (or no
/// longer has one) says. The caller knows the session it named.
const NO_BROWSER: &str = "no browser for session";

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Session ids come over IPC and end up in file names: letters, digits, `-`
/// and `_` only.
fn check_session_id(id: &str) -> Result<(), String> {
    let valid = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if valid {
        Ok(())
    } else {
        Err(format!("invalid session id {id:?}"))
    }
}

/// The Chromium of one session. [`Slot::ensure`] is the proxy's `Ensure`:
/// its lock serialises launches, so concurrent first connections start one
/// browser, and it is per session, so a slow launch blocks no other tab.
struct Slot {
    session_id: String,
    /// `<instance dir>/profiles`; each launch attempt gets `<session_id>-<n>`
    /// in there.
    profiles: PathBuf,
    /// Which browser to run, read at every launch.
    settings: Arc<SettingsStore>,
    launches: AtomicU32,
    closed: AtomicBool,
    chromium: Mutex<Option<Chromium>>,
}

impl Slot {
    /// The live Chromium's CDP connection, launching or relaunching it when
    /// there is none or it died. A configured browser that cannot be
    /// launched is an error, never a fallback to another browser.
    fn ensure(&self) -> Result<Arc<Broker>, String> {
        let mut slot = lock(&self.chromium);
        if self.closed.load(Ordering::SeqCst) {
            return Err("browser closed".into());
        }
        if let Some(chromium) = slot.as_ref().filter(|c| c.is_alive()) {
            return Ok(Arc::clone(chromium.broker()));
        }
        if let Some(dead) = slot.take() {
            dead.stop(); // reaps it and removes its profile
        }
        let binaries = self.settings.launch_candidates()?;
        private_dirs(true)
            .create(&self.profiles)
            .map_err(|e| format!("browser profile dir: {e}"))?;
        let chromium = Chromium::launch(&binaries, || self.next_profile(), WINDOW.0, WINDOW.1)?;
        let broker = Arc::clone(chromium.broker());
        *slot = Some(chromium);
        drop(slot);
        Ok(broker)
    }

    /// A profile path no earlier launch of this session used.
    fn next_profile(&self) -> PathBuf {
        let n = self.launches.fetch_add(1, Ordering::Relaxed);
        self.profiles.join(format!("{}-{n}", self.session_id))
    }
}

struct Entry {
    proxy: Proxy,
    slot: Arc<Slot>,
    overlay: PathBuf,
    /// The panel's view while it is open (`commands.rs`); also what the
    /// proxy reports the agent's pages to.
    view: Arc<commands::ViewSlot>,
    /// `true` once the teardown has finished. Held for its whole duration,
    /// so concurrent [`close`](Self::close) callers wait for the first one.
    torn_down: Mutex<bool>,
}

impl Entry {
    /// Cut the agent's connections, kill the browser, delete its profile and
    /// the overlay. Waits for a launch in flight, then stops what it started.
    /// Safe to call again, also concurrently: later callers return once the
    /// first teardown is complete.
    fn close(&self) {
        let mut torn_down = lock(&self.torn_down);
        if *torn_down {
            return;
        }
        self.slot.closed.store(true, Ordering::SeqCst);
        // Cut the panel's connection before the browser it points at goes.
        let view = lock(&self.view).close();
        if let Some(view) = view {
            view.close();
        }
        self.proxy.shutdown();
        let chromium = lock(&self.slot.chromium).take();
        if let Some(chromium) = chromium {
            chromium.stop();
        }
        let _ = std::fs::remove_file(&self.overlay);
        *torn_down = true;
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Arc<Entry>>,
    /// Entries taken out of `entries` whose teardown is not finished yet.
    /// [`BrowserManager::shutdown_all`] waits for these too.
    closing: Vec<Arc<Entry>>,
}

impl State {
    /// The session's entry to tear down: moved from `entries` to `closing`,
    /// or the one already closing (whose teardown the caller then awaits).
    fn take_for_close(&mut self, session_id: &str) -> Option<Arc<Entry>> {
        if let Some(entry) = self.entries.remove(session_id) {
            self.closing.push(Arc::clone(&entry));
            return Some(entry);
        }
        self.closing
            .iter()
            .find(|e| e.slot.session_id == session_id)
            .map(Arc::clone)
    }
}

/// What a working manager has.
struct Instance {
    /// `<root>/<pid>`, private to this process: `profiles/` and the overlays.
    dir: PathBuf,
    settings: Arc<SettingsStore>,
}

/// Per-session browsers, managed as Tauri state.
pub struct BrowserManager {
    /// The working directory and the browser setting, or why there is none;
    /// then [`allocate`](Self::allocate) always fails.
    instance: Result<Instance, String>,
    state: Mutex<State>,
}

impl BrowserManager {
    /// `root` is a user-private directory (the app cache dir in production),
    /// shared by every running app instance; this one works in `<root>/<pid>`
    /// (see [`prune_stale_instances`](Self::prune_stale_instances) for the
    /// others). Fails when `root` is a symlink or the directory cannot be
    /// created.
    pub fn new(root: &Path, settings: Arc<SettingsStore>) -> Result<Self, String> {
        let dir = prepare_instance_dir(root)
            .map_err(|e| format!("browser dir {}: {e}", root.display()))?;
        Ok(Self {
            instance: Ok(Instance { dir, settings }),
            state: Mutex::new(State::default()),
        })
    }

    /// A manager that has no working directory: every
    /// [`allocate`](Self::allocate) fails with `reason`, so sessions start
    /// without a managed browser.
    pub fn unavailable(reason: String) -> Self {
        Self {
            instance: Err(reason),
            state: Mutex::new(State::default()),
        }
    }

    /// Delete what crashed earlier app runs left under the root, after
    /// reaping their browsers; other live instances' directories stay. Can
    /// block for seconds: the app runs it on a thread of its own at startup.
    pub fn prune_stale_instances(&self) {
        if let Some(root) = self.instance.as_ref().ok().and_then(|i| i.dir.parent()) {
            prune_dead(root);
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        lock(&self.state)
    }

    /// Bind the session's placeholder port and write the overlay file omp
    /// takes as `--config=<path>`; returns its path. Idempotent: a session
    /// that already has a browser keeps it. Chromium itself starts on the
    /// first connection to the placeholder (or the panel's open).
    pub fn allocate(&self, session_id: &str) -> Result<PathBuf, String> {
        let instance = self.instance.as_ref().map_err(Clone::clone)?;
        check_session_id(session_id)?;
        let mut state = self.state();
        if let Some(entry) = state.entries.get(session_id) {
            return Ok(entry.overlay.clone()); // the caller owns the path it gets
        }
        if state
            .closing
            .iter()
            .any(|e| e.slot.session_id == session_id)
        {
            return Err(format!("browser of session {session_id} is still closing"));
        }
        let slot = Arc::new(Slot {
            session_id: session_id.to_owned(),
            profiles: instance.dir.join("profiles"),
            settings: Arc::clone(&instance.settings),
            launches: AtomicU32::new(0),
            closed: AtomicBool::new(false),
            chromium: Mutex::new(None),
        });
        let launcher = Arc::clone(&slot);
        let view = Arc::new(commands::ViewSlot::default());
        let reporter = Arc::clone(&view);
        let proxy = Proxy::bind(
            Arc::new(move || launcher.ensure()),
            Arc::new(move |target| lock(&reporter).agent_worked_in(target)),
        )
        .map_err(|e| format!("browser proxy: {e}"))?;
        let overlay = instance.dir.join(format!("{session_id}.yml"));
        write_overlay(&overlay, proxy.port()).map_err(|e| format!("browser overlay: {e}"))?;
        state.entries.insert(
            session_id.to_owned(),
            Arc::new(Entry {
                proxy,
                slot,
                overlay: overlay.clone(), // the entry keeps it for `close`
                torn_down: Mutex::new(false),
                view,
            }),
        );
        drop(state);
        Ok(overlay)
    }

    /// [`allocate`](Self::allocate), or `None` when this app run has no
    /// managed browsers at all ([`unavailable`](Self::unavailable), logged
    /// once at startup): omp then runs without the overlay. A tab whose own
    /// browser cannot be set up (still closing, the port or overlay failed)
    /// is an error, never a silent start without it.
    pub fn allocate_if_available(&self, session_id: &str) -> Result<Option<PathBuf>, String> {
        if self.instance.is_err() {
            return Ok(None);
        }
        self.allocate(session_id).map(Some)
    }

    /// The placeholder port omp connects to.
    // The tests read it; omp learns the port from the overlay file.
    #[cfg(test)]
    pub fn port(&self, session_id: &str) -> Option<u16> {
        self.state()
            .entries
            .get(session_id)
            .map(|entry| entry.proxy.port())
    }

    /// The live Chromium's CDP connection, launching it when needed (the
    /// panel's CDP client attaches here, not through the proxy).
    pub fn chromium(&self, session_id: &str) -> Result<Arc<Broker>, String> {
        let slot = self
            .state()
            .entries
            .get(session_id)
            .map(|entry| Arc::clone(&entry.slot))
            .ok_or_else(|| NO_BROWSER.to_owned())?;
        slot.ensure() // outside the map lock: a launch must not block other tabs
    }

    /// [`release_many`](Self::release_many) for one session (the tests').
    #[cfg(test)]
    pub fn release(&self, session_id: &str) {
        self.release_many(&[session_id]);
    }

    /// End these sessions' browsers (see [`Entry::close`]), side by side:
    /// closing many tabs waits for the slowest teardown, not their sum.
    /// Idempotent; returns only once each teardown is complete, also one
    /// another thread started.
    pub fn release_many<S: AsRef<str>>(&self, session_ids: &[S]) {
        let entries: Vec<_> = {
            let mut state = self.state();
            session_ids
                .iter()
                .filter_map(|id| state.take_for_close(id.as_ref()))
                .collect()
        };
        self.finish_all(&entries);
    }

    /// Release every session, including the ones whose release is still in
    /// flight (a close waiting for a slow launch), all at once. For app exit,
    /// where managed state is not dropped.
    pub fn shutdown_all(&self) {
        let all = {
            let mut state = self.state();
            let live: Vec<_> = state.entries.drain().map(|(_, entry)| entry).collect();
            state.closing.extend(live);
            // Keep shared entries alive while teardown runs outside the map
            // lock; concurrent release callers must still find and await them.
            state.closing.clone()
        };
        self.finish_all(&all);
        if let Ok(instance) = &self.instance {
            let _ = remove_dir_retrying(&instance.dir);
        }
    }

    /// [`finish_close`](Self::finish_close) every entry, in parallel when
    /// there is more than one.
    fn finish_all(&self, entries: &[Arc<Entry>]) {
        match entries {
            [] => {}
            [entry] => self.finish_close(entry),
            _ => std::thread::scope(|scope| {
                for entry in entries {
                    scope.spawn(|| self.finish_close(entry));
                }
            }),
        }
    }

    /// Tear the entry down (waiting for a concurrent teardown), then forget
    /// it. The map lock is not held meanwhile.
    fn finish_close(&self, entry: &Arc<Entry>) {
        entry.close();
        self.state().closing.retain(|e| !Arc::ptr_eq(e, entry));
    }

    /// Kills the session's Chromium but leaves the entry, as a crash would.
    #[cfg(test)]
    fn kill_chromium_for_test(&self, session_id: &str) {
        let slot = self
            .state()
            .entries
            .get(session_id)
            .map(|e| Arc::clone(&e.slot));
        if let Some(slot) = slot {
            if let Some(chromium) = lock(&slot.chromium).as_ref() {
                chromium.stop();
            }
        }
    }

    /// Profile directory of the session's most recent launch attempt.
    #[cfg(test)]
    fn profile_dir_for_test(&self, session_id: &str) -> Option<PathBuf> {
        let slot = self
            .state()
            .entries
            .get(session_id)
            .map(|e| Arc::clone(&e.slot))?;
        let n = slot.launches.load(Ordering::Relaxed).checked_sub(1)?;
        Some(slot.profiles.join(format!("{session_id}-{n}")))
    }
}

impl Drop for BrowserManager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}
