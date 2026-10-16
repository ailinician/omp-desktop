//! A setting that names one program: the terminal's shell (`terminal.json`
//! `shell`) and the browser's Chromium (`browser.json` `path`), and the path
//! rules both follow.
//!
//! ```json
//! { "<key>": "" }
//! ```
//!
//! The value is an absolute path to an executable file, or empty for the
//! feature's default. It is cached in memory: reads never touch the disk.
//! Same failure policy as the keybinding overlay (`keybindings/overlay.rs`):
//! an unreadable or malformed file reads as the default, but a change refuses
//! to overwrite it — that would silently discard whatever the user had stored
//! there. The file's other keys are kept.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde_json::{Map, Value};

use crate::json_store;

/// One program setting: its file and the in-memory copy.
pub struct ProgramSetting {
    /// `None` when the app has no config folder: the setting then lives in
    /// memory only. Never a shared fallback folder such as the temp dir,
    /// where another account could plant the program this one runs.
    file: Option<StoreFile>,
    /// The JSON key holding the value.
    key: &'static str,
    /// The stored value. Held only to copy or replace the string.
    value: Mutex<String>,
}

struct StoreFile {
    path: PathBuf,
    /// `path.with_extension("lock")`, computed once.
    lock: PathBuf,
}

impl ProgramSetting {
    /// Read `key` from `file` once (a missing or unreadable file reads as
    /// unset).
    pub fn load(file: Option<PathBuf>, key: &'static str) -> Self {
        let value = file.as_deref().map_or_else(String::new, |path| {
            json_store::read_strict::<Map<String, Value>>(path)
                .and_then(|stored| current(&stored, key).map(str::to_owned))
                .unwrap_or_else(|e| {
                    eprintln!(
                        "[omp-desktop] {} unreadable, using the default: {e}",
                        path.display()
                    );
                    String::new()
                })
        });
        Self {
            file: file.map(|path| StoreFile {
                lock: path.with_extension("lock"),
                path,
            }),
            key,
            value: Mutex::new(value),
        }
    }

    /// The cache. A panic while it was held cannot leave a half-written
    /// `String` behind (it is only ever cloned or replaced), so poison is
    /// ignored.
    fn cached(&self) -> MutexGuard<'_, String> {
        self.value.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The stored value; empty = the default.
    pub fn get(&self) -> String {
        self.cached().clone()
    }

    /// Store `value`, which the caller has already normalised and validated
    /// (empty = the default). Refuses to replace a file it cannot read (also
    /// one whose key holds something other than a string); a value equal to
    /// the stored one writes nothing. Blocking file I/O.
    pub fn set(&self, value: &str) -> Result<(), String> {
        let Some(StoreFile { path, lock }) = &self.file else {
            value.clone_into(&mut self.cached());
            return Ok(());
        };
        let key = self.key;
        json_store::update_strict(
            path,
            lock,
            |stored: &mut Map<String, Value>| {
                // A non-string is left alone here and refused in `commit`.
                let changed = current(stored, key).is_ok_and(|current| current != value);
                if changed {
                    stored.insert(key.to_owned(), Value::from(value));
                }
                changed
            },
            // Under the file lock, so the cache follows the file's order. A
            // non-string value makes the file unreadable as a setting, so
            // the refusal names the file like `update_strict`'s own.
            |stored| {
                current(&stored, key)
                    .map(|now| now.clone_into(&mut self.cached()))
                    .map_err(|e| format!("{} is unreadable: {e}", path.display()))
            },
        )?
    }
}

/// The value `stored` holds under `key`: a string, or unset when absent.
fn current<'a>(stored: &'a Map<String, Value>, key: &str) -> Result<&'a str, String> {
    match stored.get(key) {
        None => Ok(""),
        Some(Value::String(value)) => Ok(value),
        Some(other) => Err(format!("`{key}` is not a string: {other}")),
    }
}

/// A path setting as the user typed or pasted it: trimmed, and without one
/// pair of surrounding double quotes (Windows Explorer's "Copy as path"
/// adds them). Empty means "unset".
pub fn typed_path(input: &str) -> &str {
    let input = input.trim();
    input
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .map_or(input, str::trim)
}

/// An absolute path to an executable file: what a program setting must be,
/// and what a PATH search takes.
///
/// Unix: one this user may execute, as `execvp` checks it, so an execute bit
/// only for others doesn't count. Windows: any file.
pub fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        if !(path.is_absolute() && path.is_file()) {
            return false;
        }
        let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
            return false; // an interior NUL names no file
        };
        // SAFETY: `path` is a NUL-terminated string that outlives the call,
        // and `access` only reads it.
        unsafe { libc::access(path.as_ptr(), libc::X_OK) == 0 }
    }
    #[cfg(not(unix))]
    {
        path.is_absolute() && path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tempdir;

    /// Windows Explorer's "Copy as path" quotes the path; one surrounding
    /// pair is dropped, inner or unbalanced quotes stay.
    #[test]
    fn a_typed_path_loses_whitespace_and_one_pair_of_quotes() {
        for (typed, expected) in [
            ("  /usr/bin/fish ", "/usr/bin/fish"),
            (
                r#""C:\Program Files\Git\bin\bash.exe""#,
                r"C:\Program Files\Git\bin\bash.exe",
            ),
            (r#" " /x " "#, "/x"),
            (r#""/x"#, r#""/x"#),
            (r#"/a"b"#, r#"/a"b"#),
            (r#""""#, ""),
            ("", ""),
        ] {
            assert_eq!(typed_path(typed), expected, "{typed:?}");
        }
    }

    #[test]
    fn only_absolute_paths_to_executable_files_qualify() {
        let exe = std::env::current_exe().unwrap();
        assert!(is_executable_file(&exe));
        assert!(!is_executable_file(exe.parent().unwrap()), "a folder");
        assert!(!is_executable_file(Path::new("sh")), "relative");
        assert!(!is_executable_file(&exe.with_extension("missing")));
    }

    /// An execute bit for the group or others only: `execvp` would refuse
    /// it, so a PATH search must pass over it for the next folder. Not
    /// checkable as root, who may execute any file with an execute bit.
    #[cfg(unix)]
    #[test]
    fn an_execute_bit_for_others_only_does_not_qualify() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_support::TempDir::new("program-setting", "others-exec");
        let probe = dir.path().join("unreadable");
        std::fs::write(&probe, "").unwrap();
        std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&probe).is_ok() {
            return; // root: permission bits don't bind
        }
        let file = crate::test_support::executable(&dir.path().join("prog"), "");
        assert!(is_executable_file(&file));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o611)).unwrap();
        assert!(!is_executable_file(&file));
    }

    #[test]
    fn a_value_persists_and_other_keys_survive() {
        let file = tempdir("program-setting", "persist").join("setting.json");
        std::fs::write(&file, br#"{ "other": 1 }"#).unwrap();
        let setting = ProgramSetting::load(Some(file.clone()), "program");
        assert_eq!(setting.get(), "");
        setting.set("/bin/x").unwrap();
        assert_eq!(setting.get(), "/bin/x");
        let raw: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(raw, serde_json::json!({ "other": 1, "program": "/bin/x" }));
        assert_eq!(ProgramSetting::load(Some(file), "program").get(), "/bin/x");
    }

    #[test]
    fn an_unchanged_value_writes_nothing() {
        let file = tempdir("program-setting", "noop").join("setting.json");
        ProgramSetting::load(Some(file.clone()), "program")
            .set("")
            .unwrap();
        assert!(!file.exists(), "a no-op never creates the file");
    }

    #[test]
    fn an_unreadable_file_reads_as_unset_and_is_never_overwritten() {
        let dir = tempdir("program-setting", "corrupt");
        for (name, content) in [
            ("bad.json", &b"{ not json"[..]),
            ("typed.json", br#"{ "program": 7 }"#),
        ] {
            let file = dir.join(name);
            std::fs::write(&file, content).unwrap();
            let setting = ProgramSetting::load(Some(file.clone()), "program");
            assert_eq!(setting.get(), "", "{name}");
            let err = setting.set("/bin/x").expect_err(name);
            assert!(
                err.contains(&file.display().to_string()),
                "{name}: the error names the file: {err}"
            );
            assert_eq!(setting.get(), "", "{name}: cache unchanged");
            assert_eq!(std::fs::read(&file).unwrap(), content, "{name}");
        }
    }

    #[test]
    fn without_a_file_the_value_lives_in_memory() {
        let setting = ProgramSetting::load(None, "program");
        setting.set("/bin/x").unwrap();
        assert_eq!(setting.get(), "/bin/x");
    }
}
