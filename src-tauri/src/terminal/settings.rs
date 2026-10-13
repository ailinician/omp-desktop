//! The terminal's persisted settings: `<app config dir>/terminal.json`.
//!
//! ```json
//! { "shell": "" }
//! ```
//!
//! `shell` is the user's shell (an absolute path to an executable file), or
//! empty for the default ([`shell::default_shell`]). The value is cached in
//! memory: `open` reads the cache, never the disk.
//!
//! Same failure policy as the keybinding overlay
//! (`keybindings/overlay.rs`): an unreadable or malformed file reads as the
//! default, but a change refuses to overwrite it — that would silently
//! discard whatever the user had stored there.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use super::shell;
use crate::json_store;

/// The on-disk shape. Unknown keys are dropped on the next write; add a
/// field here before writing a new one.
#[derive(Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    shell: String,
}

/// What `terminal_settings` / `terminal_set_shell` return.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    /// The stored setting; empty = the default shell.
    pub shell: String,
    /// The program an empty setting runs (the Tweaks field's placeholder).
    pub default_shell: String,
}

/// `terminal.json` plus its in-memory copy.
pub struct SettingsStore {
    /// `None` when the app has no config folder: the setting then lives in
    /// memory only. Never a shared fallback folder such as the temp dir,
    /// where another account could plant the shell this one runs.
    file: Option<StoreFile>,
    /// The stored `shell`. Held only to copy or replace the string.
    shell: Mutex<String>,
}

struct StoreFile {
    path: PathBuf,
    /// `path.with_extension("lock")`, computed once.
    lock: PathBuf,
}

impl SettingsStore {
    /// Read `path` once (a missing or unreadable file reads as the default).
    pub fn load(path: Option<PathBuf>) -> Self {
        let shell = path.as_deref().map_or_else(String::new, |path| {
            json_store::read_strict::<SettingsFile>(path).map_or_else(
                |e| {
                    eprintln!(
                        "[omp-desktop] {} unreadable, using the default shell: {e}",
                        path.display()
                    );
                    String::new()
                },
                |file| file.shell,
            )
        });
        Self {
            file: path.map(|path| StoreFile {
                lock: path.with_extension("lock"),
                path,
            }),
            shell: Mutex::new(shell),
        }
    }

    /// The cache. A panic while it was held cannot leave a half-written
    /// `String` behind (it is only ever cloned or replaced), so poison is
    /// ignored.
    fn cached(&self) -> MutexGuard<'_, String> {
        self.shell.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored shell; empty = the default.
    pub fn shell(&self) -> String {
        self.cached().clone()
    }

    /// The stored shell and what the default resolves to. The default may
    /// query the user database (`getpwuid`): call off the IPC thread.
    pub fn settings(&self) -> TerminalSettings {
        TerminalSettings {
            shell: self.shell(),
            default_shell: shell::default_shell().display().to_string(),
        }
    }

    /// Store `shell` (trimmed; empty = the default) after checking it is an
    /// absolute path to an executable file, and return the new settings.
    /// Refuses to replace a file it cannot read. Blocking file I/O.
    pub fn set_shell(&self, shell: &str) -> Result<TerminalSettings, String> {
        let shell = shell.trim();
        if !shell.is_empty() {
            shell::validate_shell(shell)?;
        }
        let Some(StoreFile { path, lock }) = &self.file else {
            shell.clone_into(&mut self.cached());
            return Ok(self.settings());
        };
        json_store::update_strict(
            path,
            lock,
            |file: &mut SettingsFile| {
                let changed = file.shell != shell;
                if changed {
                    shell.clone_into(&mut file.shell);
                }
                changed
            },
            // Under the file lock, so the cache follows the file's order.
            |file| *self.cached() = file.shell,
        )?;
        Ok(self.settings())
    }
}
