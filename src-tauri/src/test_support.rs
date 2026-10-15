//! Helpers the test suites of several modules share.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Poll `check` every 10 ms until it returns `Some`, for at most `timeout`.
pub fn eventually<T>(timeout: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Poll `condition` until it holds (`true`) or `timeout` passes (`false`).
pub fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    eventually(timeout, || condition().then_some(())).is_some()
}

/// An executable stand-in at `path` holding `content` (unix: mode 0755),
/// its folder created first.
pub fn executable(path: &Path, content: &str) -> PathBuf {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(path, content).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path.to_path_buf()
}
