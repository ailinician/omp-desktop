//! Desktop-private keybinding overlay: the only file this application ever
//! writes for keyboard shortcuts.
//!
//! The overlay is plain JSON (not YAML). `serde_json` is already a dependency;
//! writing YAML would require a new crate or a hand-rolled emitter for no
//! user-visible gain, because omp never reads this file.
//!
//! On-disk shape (one named field so a future key can be added without a
//! format break):
//!
//! ```json
//! { "bindings": { "desktop.panel.changes": ["ctrl+g"], "app.plan.toggle": [] } }
//! ```
//!
//! **A malformed or unreadable file is an `Err`, never silently replaced.**
//! Replacing it would discard every rebind the user made. The error is
//! surfaced to the Shortcuts screen; dispatch still works because the frontend
//! falls back to omp + defaults.
//!
//! Without a resolvable config folder the overlay lives in memory for the
//! app run and no file is read or written.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};

use crate::json_store;
use crate::keybindings::{chord, Bindings, ACTION_IDS};

// ── on-disk representation ────────────────────────────────────────────────────

// The one-level `{ bindings: … }` wrapper was chosen so a future top-level
// field (e.g. `version`, `migrated_at`) can be added without a format break.
// Note: unknown top-level fields are currently *dropped* on read because
// `OverlayFile` has no `#[serde(flatten)]` catch-all; when adding a new field,
// add it to this struct first before writing it to disk.
#[derive(Default, Serialize, Deserialize)]
struct OverlayFile {
    #[serde(default)]
    bindings: Bindings,
}

// ── OverlayStore ─────────────────────────────────────────────────────────────

/// Manages `<app config dir>/keybindings.json`, the desktop-private overlay
/// that stores user rebinds for actions in the desktop's own registry.
pub struct OverlayStore {
    backing: Backing,
}

enum Backing {
    /// The overlay file and `path.with_extension("lock")`, computed once at
    /// construction rather than allocated on every `set`/`remove` — same
    /// precedent as `ProfileStore::load`'s own `lock` field.
    File { path: PathBuf, lock: PathBuf },
    /// No app config folder: rebinds last for the app run.
    Memory(Mutex<Bindings>),
}

impl OverlayStore {
    /// Create a store pointing at `path` (the file need not exist yet), or
    /// a memory-only one for `None`.
    pub fn new(path: Option<PathBuf>) -> Self {
        let backing = path.map_or_else(
            || Backing::Memory(Mutex::default()),
            |path| Backing::File {
                lock: path.with_extension("lock"),
                path,
            },
        );
        Self { backing }
    }

    /// The absolute path of the overlay file (used by the Shortcuts footer);
    /// `None` for a memory-only store.
    pub fn path(&self) -> Option<&Path> {
        match &self.backing {
            Backing::File { path, .. } => Some(path),
            Backing::Memory(_) => None,
        }
    }

    /// Read the current overlay. A missing file returns an empty map; a
    /// malformed or unreadable file returns `Err`.
    pub fn read(&self) -> Result<Bindings, String> {
        match &self.backing {
            Backing::File { path, .. } => {
                json_store::read_strict::<OverlayFile>(path).map(|f| f.bindings)
            }
            // The caller owns its map (it goes into the IPC payload) while
            // the store keeps its own.
            Backing::Memory(bindings) => Ok(locked(bindings).clone()),
        }
    }

    /// Set `action` → `keys` in the overlay and return the updated map.
    ///
    /// Rejects:
    /// - an unknown action (`action` not in `ACTION_IDS`)
    /// - a chord that canonicalises to an empty base (invalid)
    ///
    /// An empty `keys` slice stores `[]` — the explicit "disabled" state that
    /// beats both omp's config and the registry default.
    ///
    /// Refuses to write if the current file is malformed.
    pub fn set(&self, action: &str, keys: &[String]) -> Result<Bindings, String> {
        if !ACTION_IDS.contains(&action) {
            return Err("unknown action".to_string());
        }
        // Validate and canonicalise in one pass, de-duplicating by simple
        // linear scan rather than a `BTreeSet` — `keys` is a couple of
        // chords at most (one rebind, or the Shortcuts modal's "+ chord"
        // append), so `Vec::contains` costs nothing and needs no clone to
        // check membership before pushing.
        let mut canonical: Vec<String> = Vec::with_capacity(keys.len());
        for k in keys {
            let c = chord::canonical_chord(k);
            if c.is_empty() {
                return Err(format!("invalid chord: {k:?}"));
            }
            // De-duplicate while keeping first-seen order — the Shortcuts
            // modal's "+ chord" path can re-record a chord the action
            // already has (plan §8), which must not persist as a repeat.
            if !canonical.contains(&c) {
                canonical.push(c);
            }
        }

        self.mutate(|bindings| {
            bindings.insert(action.to_string(), canonical);
            true
        })
    }

    /// Remove `action` from the overlay (a no-op when absent, including no
    /// file write) and return the updated map.
    ///
    /// Refuses to write if the current file is malformed.
    pub fn remove(&self, action: &str) -> Result<Bindings, String> {
        self.mutate(|bindings| bindings.remove(action).is_some())
    }

    // ── internal ─────────────────────────────────────────────────────────────

    /// Read, modify with `f`, write atomically **only when `f` reports a
    /// change**, return the resulting bindings. `f` returns whether it
    /// mutated the map; a no-op `remove` of an absent action must not
    /// rewrite the file (or create it, for a fresh install) with identical
    /// content.
    fn mutate(&self, f: impl FnOnce(&mut Bindings) -> bool) -> Result<Bindings, String> {
        match &self.backing {
            Backing::File { path, lock } => json_store::update_strict(
                path,
                lock,
                |file: &mut OverlayFile| f(&mut file.bindings),
                |file| file.bindings,
            ),
            Backing::Memory(bindings) => {
                let mut bindings = locked(bindings);
                f(&mut bindings);
                // Same as `read`: the caller's copy for the payload.
                Ok(bindings.clone())
            }
        }
    }
}

/// A memory-only overlay. Every change is a single map operation, so the
/// map is consistent even after a panic elsewhere: poison is ignored.
fn locked(bindings: &Mutex<Bindings>) -> MutexGuard<'_, Bindings> {
    bindings.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Scratch directory removed on drop, including on a panicking assertion
    /// — the manual `cleanup(&dir)` calls this replaces would otherwise leak
    /// a temp directory per failed test.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// `pre_create: true` creates the scratch directory up front (the
    /// common case); `false` leaves it absent — proves `set` creates the
    /// parent directory itself on a fresh install, rather than relying on a
    /// fixture that always pre-creates it.
    fn make_store(pre_create: bool) -> (OverlayStore, Scratch) {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Include the process id so parallel test processes (`cargo test`
        // running twice concurrently) don't collide and delete each other's
        // scratch directories — same pattern as profiles.rs and json_store.rs.
        let dir = std::env::temp_dir().join(format!("omp-overlay-{}-{}", std::process::id(), id));
        let _ = fs::remove_dir_all(&dir);
        if pre_create {
            fs::create_dir_all(&dir).expect("create test dir");
        }
        let path = dir.join("keybindings.json");
        (OverlayStore::new(Some(path)), Scratch(dir))
    }

    impl OverlayStore {
        /// The overlay file of a [`make_store`] store.
        fn file(&self) -> &Path {
            self.path().expect("a file-backed store")
        }
    }

    #[test]
    fn missing_file_reads_as_empty() {
        let (store, _dir) = make_store(true);
        assert_eq!(store.read().unwrap().len(), 0);
    }

    #[test]
    fn empty_object_file_reads_as_empty() {
        let (store, _dir) = make_store(true);
        fs::write(store.file(), b"{}").unwrap();
        assert_eq!(store.read().unwrap().len(), 0);
    }

    #[test]
    fn read_parses_the_documented_on_disk_shape() {
        // Every other test reaches `read()` only after a `set()` round-trip,
        // which never pins the literal `{ "bindings": { … } }` shape the
        // module doc comment promises — a hand-edited overlay (the doc
        // footer prints the path specifically to invite this) or a stray
        // `#[serde(default)]`/field-rename regression could silently break
        // that contract while every `set`-based test stayed green.
        let (store, _dir) = make_store(true);
        fs::write(
            store.file(),
            br#"{ "bindings": { "desktop.panel.changes": ["ctrl+g"], "app.plan.toggle": [] } }"#,
        )
        .unwrap();
        let read = store.read().unwrap();
        assert_eq!(read["desktop.panel.changes"], ["ctrl+g"]);
        assert_eq!(read["app.plan.toggle"], [] as [String; 0]);
    }

    #[test]
    fn fresh_config_dir_is_created_on_first_set() {
        let (store, _dir) = make_store(false);
        assert!(!store.file().exists());
        let result = store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        assert_eq!(result["desktop.panel.changes"], ["ctrl+g"]);
        assert!(store.file().exists());
    }

    #[test]
    fn set_round_trips() {
        let (store, _dir) = make_store(true);
        let result = store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        assert_eq!(result["desktop.panel.changes"], ["ctrl+g"]);
        // Survives a fresh read.
        let read = store.read().unwrap();
        assert_eq!(read["desktop.panel.changes"], ["ctrl+g"]);
    }

    #[test]
    fn set_canonicalises_chords() {
        let (store, _dir) = make_store(true);
        store
            .set("desktop.panel.changes", &["Ctrl+G".to_string()])
            .unwrap();
        assert_eq!(store.read().unwrap()["desktop.panel.changes"], ["ctrl+g"]);
    }

    #[test]
    fn set_deduplicates_equivalent_chords() {
        let (store, _dir) = make_store(true);
        let result = store
            .set(
                "desktop.panel.changes",
                &["Ctrl+G".to_string(), "ctrl+g".to_string()],
            )
            .unwrap();
        assert_eq!(result["desktop.panel.changes"], ["ctrl+g"]);
    }

    #[test]
    fn set_empty_keys_stores_explicit_disabled() {
        let (store, _dir) = make_store(true);
        store.set("desktop.panel.changes", &[]).unwrap();
        let read = store.read().unwrap();
        assert!(read.contains_key("desktop.panel.changes"));
        assert_eq!(read["desktop.panel.changes"], [] as [String; 0]);
    }

    #[test]
    fn remove_of_absent_action_is_noop() {
        let (store, _dir) = make_store(true);
        store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        let before = fs::read(store.file()).unwrap();
        // Remove a key that was never set — must not error, must not disturb
        // others, and must not rewrite the file at all.
        store.remove("desktop.panel.todo").unwrap();
        assert!(store.read().unwrap().contains_key("desktop.panel.changes"));
        assert_eq!(
            fs::read(store.file()).unwrap(),
            before,
            "removing an absent action must not touch the file"
        );
    }

    #[test]
    fn remove_on_fresh_store_does_not_create_file() {
        let (store, _dir) = make_store(false);
        store.remove("desktop.panel.changes").unwrap();
        assert!(
            !store.file().exists(),
            "a no-op remove must not create the overlay file"
        );
    }

    #[test]
    fn two_sequential_sets_both_survive() {
        let (store, _dir) = make_store(true);
        store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        store
            .set("desktop.panel.todo", &["ctrl+u".to_string()])
            .unwrap();
        let read = store.read().unwrap();
        assert_eq!(read["desktop.panel.changes"], ["ctrl+g"]);
        assert_eq!(read["desktop.panel.todo"], ["ctrl+u"]);
    }

    #[test]
    fn set_on_unknown_action_errors_and_leaves_file_untouched() {
        let (store, _dir) = make_store(true);
        store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        let err = store
            .set("not.a.real.action", &["ctrl+x".to_string()])
            .unwrap_err();
        assert!(err.contains("unknown action"), "got: {err}");
        // The previously-written file is intact.
        assert_eq!(store.read().unwrap()["desktop.panel.changes"], ["ctrl+g"]);
    }

    #[test]
    fn invalid_chord_errors_without_writing() {
        let (store, _dir) = make_store(true);
        let err = store
            .set("desktop.panel.changes", &["ctrl+".to_string()])
            .unwrap_err();
        assert!(err.contains("invalid chord"), "got: {err}");
        // Nothing was written.
        assert_eq!(store.read().unwrap().len(), 0);
    }

    #[test]
    fn corrupt_file_makes_read_and_set_error_without_rewriting() {
        let (store, _dir) = make_store(true);
        fs::write(store.file(), b"this is not json").unwrap();
        // read() must error.
        assert!(store.read().is_err());
        let before = fs::read(store.file()).unwrap();
        // set() must also error and leave the bytes unchanged.
        store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap_err();
        let after = fs::read(store.file()).unwrap();
        assert_eq!(before, after, "corrupt file must not be overwritten");
        // remove() must error too.
        store.remove("desktop.panel.changes").unwrap_err();
        assert_eq!(fs::read(store.file()).unwrap(), before);
    }

    #[test]
    fn set_then_remove_leaves_key_absent() {
        let (store, _dir) = make_store(true);
        store
            .set("desktop.panel.changes", &["ctrl+g".to_string()])
            .unwrap();
        store.remove("desktop.panel.changes").unwrap();
        assert!(!store.read().unwrap().contains_key("desktop.panel.changes"));
    }

    #[test]
    fn a_memory_only_store_keeps_rebinds_for_its_lifetime_only() {
        let store = OverlayStore::new(None);
        assert_eq!(store.path(), None);
        assert_eq!(store.read().unwrap().len(), 0);
        let set = store
            .set("desktop.panel.changes", &["Ctrl+G".to_string()])
            .unwrap();
        assert_eq!(set["desktop.panel.changes"], ["ctrl+g"]);
        assert_eq!(store.read().unwrap(), set);
        assert!(store.set("not.an.action", &[]).is_err());
        let removed = store.remove("desktop.panel.changes").unwrap();
        assert!(removed.is_empty());
        assert_eq!(OverlayStore::new(None).read().unwrap().len(), 0);
    }
}
