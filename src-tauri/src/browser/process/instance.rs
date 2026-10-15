//! The files this app instance keeps: its private directory `<root>/<pid>`
//! (overlays, `profiles/`, a pid file per launched browser), the overlay omp
//! reads its `cdpUrl` from, and pruning what crashed earlier runs left
//! behind.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::reap;
use crate::agent::supervisor::pid_is_alive;

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
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    let written = file.write_all(contents);
    if written.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    written
}

/// The settings overlay omp takes as `--config=<path>`: its browser tool
/// attaches to the session's placeholder port. `relay: false` because omp
/// ranks an enabled browser relay above `cdpUrl`; without it a user with
/// the relay on would drive their own Chrome, not the panel's browser.
/// (A tool call's explicit `app.relay`/`app.tern`/`app.cdp_url` and the
/// `PI_BROWSER_RELAY` env var still outrank any setting.)
pub fn overlay(port: u16) -> String {
    format!("browser:\n  cdpUrl: \"http://127.0.0.1:{port}\"\n  relay: false\n")
}

/// Write the [`overlay`] for `port` to `path` (see [`create_private_file`]).
pub fn write_overlay(path: &Path, port: u16) -> io::Result<()> {
    create_private_file(path, overlay(port).as_bytes())
}

/// Create this process's private directory `<root>/<pid>`. `root` and the
/// instance directory must be real directories, not symlinks. A directory
/// with our own pid that already exists is a crashed earlier run's (pid
/// reuse, pid namespaces): its browsers are reaped and it is recreated
/// empty. Other instances' directories are [`prune_dead`]'s.
pub fn prepare_instance_dir(root: &Path) -> io::Result<PathBuf> {
    match std::fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(io::Error::other("not a real directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => private_dirs(true).create(root)?,
        Err(e) => return Err(e),
    }
    let instance = root.join(std::process::id().to_string());
    match std::fs::symlink_metadata(&instance) {
        Ok(meta) if meta.is_dir() => {
            if !reap::before_pruning(&instance) {
                return Err(io::Error::other(
                    "a browser of an earlier run with this pid is still running",
                ));
            }
            std::fs::remove_dir_all(&instance)?;
        }
        Ok(_) => return Err(io::Error::other("instance path is not a real directory")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    private_dirs(false).create(&instance)?;
    Ok(instance)
}

/// Delete the instance directories under `root` whose process no longer
/// exists, after reaping the browsers they recorded (best effort). Those of
/// live processes (other app instances sharing `root`) and our own are never
/// touched; neither are names that are not pids. Can block for seconds per
/// stuck browser: call off the main thread.
pub fn prune_dead(root: &Path) {
    let own = std::process::id();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let dead = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
            .is_some_and(|pid| pid != own && !pid_is_alive(pid));
        if dead && entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let path = entry.path();
            if reap::before_pruning(&path) {
                let _ = remove_dir_retrying(&path);
            }
        }
    }
}

/// `remove_dir_all`, retried on Windows for about a second: a killed
/// Chromium's GPU, network and crashpad processes can hold profile files open
/// for a moment after the browser process has exited (what puppeteer and omp
/// retry for, too). A missing directory is success.
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
    use crate::browser::tests::tempdir;

    #[cfg(unix)]
    #[test]
    fn private_files_are_exclusive_and_never_follow_a_symlink() {
        let dir = tempdir("private-file");
        let path = dir.join("profile.pid");
        create_private_file(&path, b"1234\n").unwrap();
        assert!(create_private_file(&path, b"5678\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1234\n");
        std::fs::remove_file(&path).unwrap();
        let victim = dir.join("victim");
        std::fs::write(&victim, "untouched").unwrap();
        std::os::unix::fs::symlink(&victim, &path).unwrap();
        assert!(create_private_file(&path, b"5678\n").is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "untouched");
    }

    #[test]
    fn removing_a_missing_directory_succeeds() {
        let dir = tempdir("remove-missing");
        assert!(remove_dir_retrying(&dir.join("gone")).is_ok());
        let full = dir.join("full/deeper");
        std::fs::create_dir_all(&full).unwrap();
        std::fs::write(full.join("f"), "x").unwrap();
        remove_dir_retrying(&dir.join("full")).unwrap();
        assert!(!dir.join("full").exists());
    }
}
