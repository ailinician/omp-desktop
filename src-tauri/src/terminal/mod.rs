//! Per-tab shell on a pseudo-terminal (the terminal panel's Rust side).
//!
//! `TerminalManager` owns one [`pty::Pty`] per session id. The webview sends
//! raw bytes in (`terminal_write`) and receives output as base64 events:
//! `terminal://data/{id}` `{ "b64": … }` and, once the shell has exited,
//! `terminal://exit/{id}` `{ "code": number|null }` (`null` = killed by a
//! signal). Nothing here interprets what is typed or printed.
//!
//! Lock discipline: the map is held only to look up, insert or remove an
//! entry; commands then work on their own `Arc` of it. Nothing that can block
//! runs on the IPC thread: input is queued for the entry's writer thread, and
//! killing (`open` replacing, `close`, `shutdown_all`) is done by callers
//! that are off it (`spawn_blocking`).
//!
//! Lifecycle: `open`, `close` and `shutdown_all` run on separate worker
//! threads, so they take one manager-wide lifecycle lock for their whole
//! duration. Otherwise a `close` during a pending `open` could return before
//! the fresh shell is inserted (leaving an orphan), and two overlapping opens
//! would both spawn. One lock rather than one per session: spawning takes
//! about 10 ms and tabs open or close rarely, so contention is negligible and
//! there is no lock table to prune. `write` and `resize` never take it.

mod kill;
mod pty;
mod pump;
#[cfg(all(test, unix))]
mod tests;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};

use pty::Pty;

/// Where terminal output goes. The app emits Tauri events; tests collect.
pub trait Emit: Send + Sync + 'static {
    /// A chunk of terminal output (at most 64 KiB).
    fn data(&self, id: &str, bytes: &[u8]);
    /// The shell exited; `None` when it was killed by a signal.
    fn exit(&self, id: &str, code: Option<i32>);
}

/// [`Emit`] over Tauri events.
pub struct TauriEmit(pub AppHandle);

#[derive(Serialize, Clone)]
struct DataPayload<'a> {
    b64: &'a str,
}

#[derive(Serialize, Clone)]
struct ExitPayload {
    code: Option<i32>,
}

impl Emit for TauriEmit {
    fn data(&self, id: &str, bytes: &[u8]) {
        let b64 = STANDARD.encode(bytes);
        let _ = self
            .0
            .emit(&format!("terminal://data/{id}"), DataPayload { b64: &b64 });
    }

    fn exit(&self, id: &str, code: Option<i32>) {
        let _ = self
            .0
            .emit(&format!("terminal://exit/{id}"), ExitPayload { code });
    }
}

/// Arguments of `terminal_open`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenArgs {
    session_id: String,
    /// The tab's project folder (the home folder when it has none).
    cwd: String,
    /// The user's shell setting; `None` or empty = the default shell.
    shell: Option<String>,
    cols: u16,
    rows: u16,
}

/// Managed state: the live terminals by session id.
#[derive(Default)]
pub struct TerminalManager {
    ptys: Mutex<HashMap<String, Arc<Pty>>>,
    /// Serializes `open`, `close` and `shutdown_all`; taken before `ptys`.
    lifecycle: Mutex<()>,
}

impl TerminalManager {
    fn lifecycle(&self) -> MutexGuard<'_, ()> {
        self.lifecycle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// The map. A panic while it was held cannot leave it inconsistent (it
    /// is only ever looked up, inserted into or removed from), so poison is
    /// ignored.
    fn ptys(&self) -> MutexGuard<'_, HashMap<String, Arc<Pty>>> {
        self.ptys.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn get(&self, session_id: &str) -> Result<Arc<Pty>, String> {
        self.ptys()
            .get(session_id)
            .map(Arc::clone)
            .ok_or_else(|| no_terminal(session_id))
    }

    /// Start the session's shell. A no-op while its shell is alive; after it
    /// exited the new one replaces it (and the old one's leftovers die).
    /// Blocks while spawning and killing: call off the IPC thread.
    pub fn open<E: Emit>(&self, emit: Arc<E>, args: &OpenArgs) -> Result<(), String> {
        if args.session_id.is_empty() {
            return Err("terminal session id is empty".to_owned());
        }
        if args.cols == 0 || args.rows == 0 {
            return Err(format!("invalid terminal size {}x{}", args.cols, args.rows));
        }
        if !Path::new(&args.cwd).is_dir() {
            return Err(format!("terminal folder {:?} is not a directory", args.cwd));
        }
        let _lifecycle = self.lifecycle();
        if self
            .ptys()
            .get(&args.session_id)
            .is_some_and(|pty| pty.is_alive())
        {
            return Ok(());
        }
        let (program, shell_args) = pty::resolve_shell(args.shell.as_deref())?;
        let fresh = Arc::new(pty::spawn(emit, args, &program, &shell_args)?);
        // The map owns its key; `args` is only borrowed.
        let replaced = self.ptys().insert(args.session_id.clone(), fresh);
        // Killed here, outside the lock.
        if let Some(old) = replaced {
            old.kill();
        }
        Ok(())
    }

    /// Queue `data` for the shell, exactly as given; the terminal's writer
    /// thread writes it, in order. Never blocks, which is why the command can
    /// stay synchronous: concurrent async tasks could reorder keystrokes.
    pub fn write(&self, session_id: &str, data: impl Into<Vec<u8>>) -> Result<(), String> {
        self.get(session_id)?.write(data.into())
    }

    /// Tell the shell its window is now `cols` × `rows` characters (one quick
    /// ioctl, so synchronous too, for the same ordering reason).
    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        if cols == 0 || rows == 0 {
            return Err(format!("invalid terminal size {cols}x{rows}"));
        }
        self.get(session_id)?.resize(cols, rows)
    }

    /// Kill the session's shell and everything it started. Silent: no exit
    /// event follows. Unknown sessions are ignored. Blocks briefly: call off
    /// the IPC thread.
    pub fn close(&self, session_id: &str) {
        let _lifecycle = self.lifecycle();
        let removed = self.ptys().remove(session_id);
        if let Some(pty) = removed {
            pty.kill();
        }
    }

    /// Kill every shell, synchronously — for the path where the process ends
    /// without running destructors (an update's relaunch).
    pub fn shutdown_all(&self) {
        let _lifecycle = self.lifecycle();
        let all: Vec<Arc<Pty>> = self.ptys().drain().map(|(_, pty)| pty).collect();
        for pty in all {
            pty.kill();
        }
    }
}

fn no_terminal(session_id: &str) -> String {
    format!("no terminal for session {session_id}")
}
