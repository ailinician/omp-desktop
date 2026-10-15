//! The terminal's persisted settings: `<app config dir>/terminal.json`.
//!
//! ```json
//! { "shell": "" }
//! ```
//!
//! `shell` is the user's shell (an absolute path to an executable file), or
//! empty for the default ([`shell::default_shell`]). Storage, cache and
//! failure policy are `program_setting`'s: `open` reads the cache, never the
//! disk.

use std::path::PathBuf;

use serde::Serialize;

use super::shell;
use super::shells::{self, ShellChoice};
use crate::program_setting::{typed_path, ProgramSetting};

/// What `terminal_settings` / `terminal_set_shell` return.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSettings {
    /// The stored setting; empty = the default shell.
    pub shell: String,
    /// The program an empty setting runs (the menu's `default` row).
    pub default_shell: String,
    /// The installed shells the menu offers.
    pub shells: Vec<ShellChoice>,
}

/// `terminal.json` plus its in-memory copy.
pub struct SettingsStore(ProgramSetting);

impl SettingsStore {
    /// Read `path` once (a missing or unreadable file reads as the default;
    /// `None` keeps the setting in memory only).
    pub fn load(path: Option<PathBuf>) -> Self {
        Self(ProgramSetting::load(path, "shell"))
    }

    /// The stored shell; empty = the default.
    pub fn shell(&self) -> String {
        self.0.get()
    }

    /// The stored shell, what the default resolves to and the installed
    /// shells. The default may query the user database (`getpwuid`) and the
    /// list checks files: call off the IPC thread.
    pub fn settings(&self) -> TerminalSettings {
        TerminalSettings {
            shell: self.shell(),
            default_shell: shell::default_shell().display().to_string(),
            shells: shells::available(),
        }
    }

    /// Store `shell` ([`typed_path`]: trimmed, one pair of surrounding quotes
    /// dropped; empty = the default) after checking it is an absolute path
    /// to an executable file, and return the new settings. Refuses to replace
    /// a file it cannot read. Blocking file I/O.
    pub fn set_shell(&self, shell: &str) -> Result<TerminalSettings, String> {
        let shell = typed_path(shell);
        if !shell.is_empty() {
            shell::validate_shell(shell)?;
        }
        self.0.set(shell)?;
        Ok(self.settings())
    }
}
