//! Directories private to the user, for app state another local account must
//! not read or plant files in (the browser's instance dirs, widget script
//! copies): creating them, finding the per-process ones an earlier run left
//! behind, and removing them.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A directory builder that creates `0700` directories (unix).
pub fn private_dirs(recursive: bool) -> std::fs::DirBuilder {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
}

/// Create `path` exclusively with `contents`, private to the user (unix
/// `0600`). `O_EXCL` never follows a symlink nor reuses a file somebody else
/// put there; a failed write removes the half-written file again.
pub fn create_private_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    create_and_finish(path, contents, |_| Ok(()))
}

/// Write `bytes` to `temp_path` (created exclusively with mode `0600`), sync
/// it, and rename it over `path`, removing `temp_path` on failure.
pub fn write_private_via(path: &Path, temp_path: &Path, bytes: &[u8]) -> io::Result<()> {
    create_and_finish(temp_path, bytes, |file| {
        file.sync_all()?;
        std::fs::rename(temp_path, path)
    })
}

fn create_and_finish(
    path: &Path,
    bytes: &[u8],
    after_write: impl FnOnce(std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let result = file.write_all(bytes).and_then(|()| after_write(file));
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

/// The subdirectories of `root` named by a process id, with that pid: the
/// per-process directories (`<root>/<pid>`) app instances keep, for pruning
/// those of processes that are gone. Names that are not pids are skipped.
pub fn pid_dirs(root: &Path) -> impl Iterator<Item = (u32, PathBuf)> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir())
                .then(|| (pid, entry.path()))
        })
}

/// `remove_dir_all`, retried on Windows for about a second: a file in the
/// tree can stay open for a moment after its user is gone — a killed
/// Chromium's GPU, network and crashpad processes hold profile files (what
/// puppeteer and omp retry for, too), an antivirus scans fresh script copies.
/// A missing directory is success.
pub fn remove_dir_retrying(path: &Path) -> io::Result<()> {
    const ATTEMPTS: u32 = if cfg!(windows) { 10 } else { 1 };
    const BACKOFF: Duration = Duration::from_millis(100);
    let mut attempt = 1;
    loop {
        match std::fs::remove_dir_all(path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) if attempt >= ATTEMPTS => return Err(e),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(BACKOFF);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn removing_a_missing_directory_succeeds() {
        let dir = TempDir::new("private-fs", "remove-missing");
        assert!(remove_dir_retrying(&dir.path().join("gone")).is_ok());
        let full = dir.path().join("full/deeper");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("f"), "x").unwrap();
        remove_dir_retrying(&dir.path().join("full")).unwrap();
        assert!(!dir.path().join("full").exists());
    }

    #[test]
    fn pid_dirs_lists_only_directories_named_by_a_pid() {
        let dir = TempDir::new("private-fs", "pid-dirs");
        for name in ["12", "34", "not-a-pid", "-5"] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
        }
        std::fs::write(dir.path().join("56"), "a file").unwrap();
        let mut found: Vec<_> = pid_dirs(dir.path()).collect();
        found.sort_unstable();
        assert_eq!(
            found,
            [(12, dir.path().join("12")), (34, dir.path().join("34"))]
        );
        assert_eq!(pid_dirs(&dir.path().join("missing")).count(), 0);
    }
}
