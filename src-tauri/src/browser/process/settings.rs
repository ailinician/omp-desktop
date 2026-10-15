//! The user's Chromium choice: `<app config dir>/browser.json`.
//!
//! ```json
//! { "path": "" }
//! ```
//!
//! `path` is the browser to launch (an absolute path to an executable file),
//! or empty for automatic discovery (`locate.rs`). Only Rust writes it: the
//! webview asks through `browser_set_path` / `browser_pick_path` and never
//! sends a path for a launch. A configured path that stops being executable
//! is a launch error, never a silent fallback to another browser.
//!
//! Same storage and failure policy as the terminal's shell setting
//! (`terminal/settings.rs`): cached in memory, an unreadable file reads as
//! automatic, but a change refuses to overwrite it.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use super::locate::{self, Places, Platform};
use crate::json_store;
use crate::terminal::{is_executable_file, typed_path};

/// The on-disk shape. Unknown keys are dropped on the next write; add a
/// field here before writing a new one.
#[derive(Default, Serialize, Deserialize)]
struct SettingsFile {
    #[serde(default)]
    path: String,
}

/// What `browser_settings` / `browser_set_path` / `browser_pick_path`
/// return.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserSettings {
    /// The stored setting; empty = automatic.
    pub path: String,
    /// The browser automatic discovery picks right now, if any.
    pub automatic: Option<String>,
    /// The browsers discovery finds, best first (the menu's rows).
    pub installed: Vec<String>,
    /// Why the stored path cannot be launched (it is no longer an
    /// executable file); `None` when it can, or when it is automatic.
    pub error: Option<String>,
}

/// `browser.json` plus its in-memory copy.
pub struct SettingsStore {
    /// `None` when the app has no config folder: the setting then lives in
    /// memory only (never a shared fallback folder another account could
    /// plant a browser in).
    file: Option<StoreFile>,
    /// The user's home, where omp's Chrome for Testing builds live.
    home: PathBuf,
    /// The stored `path`. Held only to copy or replace the string.
    path: Mutex<String>,
}

struct StoreFile {
    path: PathBuf,
    /// `path.with_extension("lock")`, computed once.
    lock: PathBuf,
}

impl SettingsStore {
    /// Read `file` once (a missing or unreadable file reads as automatic).
    pub fn load(file: Option<PathBuf>, home: PathBuf) -> Self {
        let path = file.as_deref().map_or_else(String::new, |file| {
            json_store::read_strict::<SettingsFile>(file).map_or_else(
                |e| {
                    eprintln!(
                        "[omp-desktop] {} unreadable, finding Chromium automatically: {e}",
                        file.display()
                    );
                    String::new()
                },
                |stored| stored.path,
            )
        });
        Self {
            file: file.map(|path| StoreFile {
                lock: path.with_extension("lock"),
                path,
            }),
            home,
            path: Mutex::new(path),
        }
    }

    /// The cache. Only ever cloned or replaced, so poison is ignored.
    fn cached(&self) -> MutexGuard<'_, String> {
        self.path.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored path; empty = automatic.
    pub fn path(&self) -> String {
        self.cached().clone()
    }

    /// The stored path, what automatic discovery resolves to and the
    /// browsers it finds. Checks files: call off the IPC thread.
    pub fn settings(&self) -> BrowserSettings {
        let path = self.path();
        let found = locate::discover(Platform::current(), &Places::current(&self.home));
        let installed: Vec<String> = found
            .binaries
            .iter()
            .map(|bin| bin.display().to_string())
            .collect();
        BrowserSettings {
            error: (!path.is_empty())
                .then(|| configured(&path).err())
                .flatten(),
            path,
            automatic: installed.first().cloned(), // the list keeps its own copy
            installed,
        }
    }

    /// Store `path` ([`typed_path`]: trimmed, one pair of surrounding quotes
    /// dropped; empty = automatic; a macOS `.app` bundle stands for its
    /// executable) after checking it is an absolute path to an executable
    /// file, and return the new settings. Refuses to replace a file it
    /// cannot read. Blocking file I/O.
    pub fn set_path(&self, path: &str) -> Result<BrowserSettings, String> {
        let typed = typed_path(path);
        let bundled = app_bundle_binary(typed);
        let path = bundled.as_deref().unwrap_or(typed);
        if !path.is_empty() {
            configured(path)?;
        }
        let Some(StoreFile { path: file, lock }) = &self.file else {
            path.clone_into(&mut self.cached());
            return Ok(self.settings());
        };
        json_store::update_strict(
            file,
            lock,
            |stored: &mut SettingsFile| {
                let changed = stored.path != path;
                if changed {
                    path.clone_into(&mut stored.path);
                }
                changed
            },
            // Under the file lock, so the cache follows the file's order.
            |stored| *self.cached() = stored.path,
        )?;
        Ok(self.settings())
    }

    /// The binaries one launch may try, best first: the configured one
    /// alone (an error when it is no longer executable: the user chose it,
    /// so no other browser stands in), else what automatic discovery finds.
    /// Checks files: call off the IPC thread.
    pub fn launch_candidates(&self) -> Result<Vec<PathBuf>, String> {
        let path = self.path();
        if !path.is_empty() {
            return configured(&path).map(|binary| vec![binary]);
        }
        let found = locate::discover(Platform::current(), &Places::current(&self.home));
        if !found.binaries.is_empty() {
            return Ok(found.binaries);
        }
        let skipped = found
            .skipped
            .iter()
            .fold(String::new(), |mut out, (bin, why)| {
                let _ = write!(out, " (skipped {}: {why})", bin.display());
                out
            });
        Err(format!(
            "no Chromium found{skipped}: install Google Chrome or Chromium, or choose a browser in Tweaks → Browser"
        ))
    }
}

/// A configured browser path: an absolute path to an executable file.
fn configured(path: &str) -> Result<PathBuf, String> {
    let binary = Path::new(path);
    if is_executable_file(binary) {
        Ok(binary.to_path_buf())
    } else {
        Err(format!(
            "the browser {path:?} set in Tweaks → Browser is not an absolute path to an executable file"
        ))
    }
}

/// The executable of the macOS app bundle `path` (`/Applications/Google
/// Chrome.app` → `…/Contents/MacOS/Google Chrome`, the layout of every
/// Chromium-based browser), or `None` when `path` is not a `.app` folder.
/// The file dialog picks a bundle as a whole.
fn app_bundle_binary(path: &str) -> Option<String> {
    let bundle = path.trim_end_matches('/');
    let name = Path::new(bundle)
        .file_name()?
        .to_str()?
        .strip_suffix(".app")?;
    Path::new(bundle)
        .is_dir()
        .then(|| format!("{bundle}/Contents/MacOS/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::tests::tempdir;

    /// An executable file in `dir` (unix: mode 0755).
    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    #[test]
    fn set_path_validates_and_normalises() {
        let dir = tempdir("browser-settings-validate");
        let store = SettingsStore::load(None, dir.clone());
        let bin = executable(&dir, "my-chrome");
        let shown = bin.display().to_string();

        // Windows "Copy as path" quotes, plus stray whitespace.
        let quoted = format!("  \"{shown}\" ");
        assert_eq!(store.set_path(&quoted).unwrap().path, shown);
        assert_eq!(store.launch_candidates().unwrap(), vec![bin]);

        // Refused, and the stored value stays.
        let not_executable = dir.join("plain.txt");
        std::fs::write(&not_executable, "x").unwrap();
        let mut refused = vec![
            "chrome".to_owned(),
            "./my-chrome".to_owned(),
            dir.join("missing").display().to_string(),
            dir.display().to_string(),
        ];
        if cfg!(unix) {
            refused.push(not_executable.display().to_string());
        }
        for bad in &refused {
            assert!(store.set_path(bad).is_err(), "{bad}");
            assert_eq!(store.path(), shown, "{bad}");
        }

        // Empty (after trimming and unquoting) = automatic.
        for automatic in ["", "   ", "\"\""] {
            store.set_path(&shown).unwrap();
            let settings = store.set_path(automatic).unwrap();
            assert_eq!(settings.path, "", "{automatic:?}");
            assert_eq!(settings.error, None);
        }
    }

    /// The macOS file dialog picks a `.app` as a whole: its executable is
    /// what gets stored.
    #[cfg(unix)]
    #[test]
    fn an_app_bundle_stands_for_its_executable() {
        let dir = tempdir("browser-settings-bundle");
        let macos = dir.join("My Browser.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let bin = executable(&macos, "My Browser");
        let store = SettingsStore::load(None, dir.clone());
        let bundle = format!("{}/", dir.join("My Browser.app").display());
        assert_eq!(
            store.set_path(&bundle).unwrap().path,
            bin.display().to_string()
        );
        assert!(store
            .set_path(&dir.join("Missing.app").display().to_string())
            .is_err());
    }

    #[test]
    fn the_setting_persists_and_a_vanished_browser_is_an_error_not_a_fallback() {
        let dir = tempdir("browser-settings-persist");
        let file = dir.join("browser.json");
        let bin = executable(&dir, "my-chrome");
        SettingsStore::load(Some(file.clone()), dir.clone())
            .set_path(&bin.display().to_string())
            .unwrap();
        let raw: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(raw["path"], bin.display().to_string());

        let reloaded = SettingsStore::load(Some(file), dir);
        assert_eq!(reloaded.path(), bin.display().to_string());
        std::fs::remove_file(&bin).unwrap();
        let err = reloaded.launch_candidates().unwrap_err();
        assert!(err.contains("Tweaks → Browser"), "{err}");
        let settings = reloaded.settings();
        assert_eq!(
            settings.path,
            bin.display().to_string(),
            "kept for the user to see"
        );
        assert!(settings.error.is_some());
    }

    #[test]
    fn an_unreadable_file_reads_as_automatic_and_is_never_overwritten() {
        let dir = tempdir("browser-settings-corrupt");
        let file = dir.join("browser.json");
        std::fs::write(&file, b"{ not json").unwrap();
        let store = SettingsStore::load(Some(file.clone()), dir.clone());
        assert_eq!(store.path(), "");
        let bin = executable(&dir, "my-chrome");
        assert!(store.set_path(&bin.display().to_string()).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"{ not json");
    }

    #[test]
    fn settings_serialise_in_camel_case() {
        let settings = BrowserSettings {
            path: String::new(),
            automatic: Some("/usr/lib/chromium/chromium".into()),
            installed: vec!["/usr/lib/chromium/chromium".into()],
            error: None,
        };
        assert_eq!(
            serde_json::to_value(&settings).unwrap(),
            serde_json::json!({
                "path": "",
                "automatic": "/usr/lib/chromium/chromium",
                "installed": ["/usr/lib/chromium/chromium"],
                "error": null,
            })
        );
    }
}
