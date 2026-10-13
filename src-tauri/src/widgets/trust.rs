//! One-time approval of widget plugins: a plugin never runs until the user
//! approved its exact [`fingerprint`] (argv plus the contents of every script
//! the argv names), and any change to either needs approval again.
//!
//! Threat model: approving happens through the webview
//! (`widgets_approve(pluginId, fingerprint)`), like tool-approval answers. A
//! compromised webview could approve, but it can already send prompts and
//! answer tool approvals, so this adds no new power. What approval defends
//! against is a config or script change made outside the desktop UI — the
//! agent editing `widgets.json` or a plugin script under write-mode approval.
//! `widget-approvals.json` is an ordinary user file (like `approval-rules/`):
//! anything with arbitrary user-file write can edit it, just as it can edit
//! `~/.bashrc`.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::json_store;
use crate::widgets::config::PluginEntry;

/// Domain separator, bumped if the hashed layout ever changes (which
/// invalidates every approval — intended).
const DOMAIN: &[u8] = b"omp-widget-v1\0";
/// Scripts larger than this contribute `len + mtime` instead of their bytes,
/// so a huge interpreter binary named in argv never costs a full read per run.
const MAX_HASHED_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// Read chunk for streaming a script into the hash.
const CHUNK_BYTES: usize = 16 * 1024;
const APPROVALS_VERSION: u32 = 1;

/// Hex SHA-256 over the argv and every regular file an absolute argv element
/// names. See the module docs for what it protects.
pub fn fingerprint(entry: &PluginEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update(len_bytes(entry.command.len()));
    for arg in &entry.command {
        hasher.update(arg.as_bytes());
        hasher.update([0]);
    }
    for (index, arg) in entry.command.iter().enumerate() {
        let path = Path::new(arg);
        if !path.is_absolute() {
            continue;
        }
        // `metadata` follows symlinks: the target's contents are what runs.
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        hasher.update(len_bytes(index));
        hasher.update(meta.len().to_le_bytes());
        if meta.len() > MAX_HASHED_FILE_BYTES {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos());
            hasher.update(b"large\0");
            hasher.update(mtime.to_le_bytes());
        } else {
            hasher.update(b"file\0");
            hash_file(path, &mut hasher);
        }
    }
    json_store::hex_lower(&hasher.finalize())
}

fn len_bytes(n: usize) -> [u8; 8] {
    u64::try_from(n).unwrap_or(u64::MAX).to_le_bytes()
}

/// Stream `path` into `hasher` one chunk at a time, then the byte count
/// actually read (the file may change between `metadata` and the read). An
/// unreadable file hashes a marker instead, so it never matches its content.
fn hash_file(path: &Path, hasher: &mut Sha256) {
    let Ok(mut file) = File::open(path) else {
        hasher.update(b"unreadable\0");
        return;
    };
    let mut buf = [0u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                total = total.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => {
                hasher.update(b"unreadable\0");
                return;
            }
        }
    }
    hasher.update(total.to_le_bytes());
}

/// Approval state of one plugin's current fingerprint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Approved,
    /// Never approved.
    Pending,
    /// Approved under another fingerprint: argv or a script changed.
    Changed,
}

#[derive(serde::Deserialize, Default)]
struct ApprovalFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    approved: BTreeMap<String, String>,
}

#[derive(serde::Serialize)]
struct ApprovalFileRef<'a> {
    version: u32,
    approved: &'a BTreeMap<String, String>,
}

/// `<app config>/widget-approvals.json`:
/// `{ "version": 1, "approved": { "<id>": "<fingerprint>" } }`.
///
/// Entries of plugins no longer in `widgets.json` are kept, so removing and
/// re-adding an unchanged plugin needs no new approval.
#[derive(Debug, Default)]
pub struct ApprovalStore {
    /// `None` when the app config dir is unknown: nothing can be approved.
    path: Option<PathBuf>,
    approved: BTreeMap<String, String>,
}

impl ApprovalStore {
    /// Read the store at `path` (missing or unreadable file = empty).
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut store = Self {
            path,
            approved: BTreeMap::new(),
        };
        store.reload();
        store
    }

    /// Re-read the file, replacing the in-memory approvals.
    pub fn reload(&mut self) {
        let Some(path) = &self.path else {
            self.approved.clear();
            return;
        };
        let file: ApprovalFile = json_store::read_or_default(path);
        self.approved = if file.version == APPROVALS_VERSION {
            file.approved
        } else {
            if file.version != 0 || !file.approved.is_empty() {
                eprintln!(
                    "[omp-desktop] {}: unsupported version {}, no widget is approved",
                    path.display(),
                    file.version
                );
            }
            BTreeMap::new()
        };
    }

    /// The fingerprint `id` was approved under, if any.
    pub fn approved(&self, id: &str) -> Option<&str> {
        self.approved.get(id).map(String::as_str)
    }

    pub fn status(&self, id: &str, fingerprint: &str) -> Approval {
        match self.approved(id) {
            Some(fp) if fp == fingerprint => Approval::Approved,
            Some(_) => Approval::Changed,
            None => Approval::Pending,
        }
    }

    /// Approve `id` under `current` (the freshly computed fingerprint) and
    /// persist atomically. `presented` is what the user saw on the approval
    /// card; a mismatch means the plugin changed after the card was drawn, so
    /// the approval is refused rather than granted for something unseen.
    ///
    /// # Errors
    /// Stale `presented`, no config directory, or a failed write (the
    /// in-memory state is then left unchanged).
    pub fn approve(&mut self, id: &str, presented: &str, current: &str) -> Result<(), String> {
        if presented != current {
            return Err(format!(
                "widget \"{id}\" changed since it was shown — review it again"
            ));
        }
        let Some(path) = &self.path else {
            return Err("no app config directory: widget approvals cannot be saved".to_owned());
        };
        let previous = self.approved.insert(id.to_owned(), current.to_owned());
        if let Err(e) = write(path, &self.approved) {
            // Roll back so memory never claims what the file does not hold.
            match previous {
                Some(fp) => {
                    self.approved.insert(id.to_owned(), fp);
                }
                None => {
                    self.approved.remove(id);
                }
            }
            return Err(e);
        }
        Ok(())
    }
}

fn write(path: &Path, approved: &BTreeMap<String, String>) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&ApprovalFileRef {
        version: APPROVALS_VERSION,
        approved,
    })
    .map_err(|e| e.to_string())?;
    json_store::write_atomic(path, &bytes).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::config::{Scope, Trigger};

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir()
                .join(format!("omp-widget-trust-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(command: &[&str]) -> PluginEntry {
        PluginEntry {
            id: "probe".to_owned(),
            title: "Probe".to_owned(),
            command: command.iter().map(|s| (*s).to_owned()).collect(),
            match_path: None,
            match_remote: None,
            triggers: vec![Trigger::Activate],
            timeout_ms: 10_000,
            interval_ms: None,
            enabled: true,
            scope: Scope::Tab,
        }
    }

    #[test]
    fn fingerprint_is_stable_and_follows_argv() {
        let a = entry(&["sh", "-c", "echo {}"]);
        assert_eq!(fingerprint(&a), fingerprint(&a));
        assert_eq!(fingerprint(&a).len(), 64);
        assert_ne!(
            fingerprint(&a),
            fingerprint(&entry(&["sh", "-c", "echo []"]))
        );
        // NUL termination keeps the argument boundaries in the hash.
        assert_ne!(
            fingerprint(&entry(&["ab", "c"])),
            fingerprint(&entry(&["a", "bc"]))
        );
    }

    #[test]
    fn fingerprint_changes_with_script_content_only() {
        let dir = Scratch::new("content");
        let script = dir.0.join("plugin.sh");
        fs::write(&script, "echo '{\"v\":1}'\n").unwrap();
        let e = entry(&["sh", script.to_str().unwrap()]);
        let before = fingerprint(&e);
        assert_eq!(
            before,
            fingerprint(&e),
            "unchanged script, unchanged fingerprint"
        );

        // Same length, different bytes: content is hashed, not just metadata.
        fs::write(&script, "echo '{\"v\":2}'\n").unwrap();
        let after = fingerprint(&e);
        assert_ne!(before, after);

        fs::write(&script, "echo '{\"v\":1}'\n").unwrap();
        assert_eq!(
            before,
            fingerprint(&e),
            "restored content restores the fingerprint"
        );
    }

    #[test]
    fn fingerprint_ignores_directories_and_missing_paths() {
        let dir = Scratch::new("dirs");
        let missing = dir.0.join("missing.sh");
        let e = entry(&["sh", missing.to_str().unwrap()]);
        let before = fingerprint(&e);
        fs::create_dir(&missing).unwrap();
        assert_eq!(before, fingerprint(&e), "a directory contributes nothing");
        fs::remove_dir(&missing).unwrap();
        fs::write(&missing, "x").unwrap();
        assert_ne!(
            before,
            fingerprint(&e),
            "a file appearing at the path counts"
        );
    }

    #[test]
    fn store_round_trips_and_reports_status() {
        let dir = Scratch::new("store");
        let path = || dir.0.join("nested").join("widget-approvals.json");
        let mut store = ApprovalStore::load(Some(path()));
        assert_eq!(store.status("probe", "aa"), Approval::Pending);
        store.approve("probe", "aa", "aa").unwrap();

        let reread = ApprovalStore::load(Some(path()));
        assert_eq!(reread.status("probe", "aa"), Approval::Approved);
        assert_eq!(reread.status("probe", "bb"), Approval::Changed);
        assert_eq!(reread.status("other", "aa"), Approval::Pending);

        let raw: serde_json::Value = serde_json::from_slice(&fs::read(path()).unwrap()).unwrap();
        assert_eq!(
            raw,
            serde_json::json!({ "version": 1, "approved": { "probe": "aa" } })
        );
    }

    #[test]
    fn approving_a_stale_fingerprint_is_rejected() {
        let dir = Scratch::new("stale");
        let script = dir.0.join("plugin.sh");
        fs::write(&script, "echo {}\n").unwrap();
        let e = entry(&["sh", script.to_str().unwrap()]);
        let shown = fingerprint(&e);
        fs::write(&script, "rm -rf ~\n").unwrap();

        let path = || dir.0.join("widget-approvals.json");
        let mut store = ApprovalStore::load(Some(path()));
        let err = store
            .approve("probe", &shown, &fingerprint(&e))
            .unwrap_err();
        assert!(err.contains("changed"), "{err}");
        assert_eq!(store.approved("probe"), None);
        assert!(!path().exists(), "a refused approval writes nothing");
    }

    #[test]
    fn unknown_version_approves_nothing() {
        let dir = Scratch::new("version");
        let path = dir.0.join("widget-approvals.json");
        fs::write(&path, r#"{"version":2,"approved":{"probe":"aa"}}"#).unwrap();
        let store = ApprovalStore::load(Some(path));
        assert_eq!(store.status("probe", "aa"), Approval::Pending);
    }

    #[test]
    fn approving_without_a_config_dir_fails() {
        let mut store = ApprovalStore::load(None);
        assert!(store.approve("probe", "aa", "aa").is_err());
        assert_eq!(store.status("probe", "aa"), Approval::Pending);
    }
}
