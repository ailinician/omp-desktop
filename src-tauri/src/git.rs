//! Git repository introspection — branch discovery and HEAD-file location.
//!
//! Uses [`gix`] (pure-Rust gitoxide) with no C-library dependency.
//! All functions are infallible from the caller's perspective: errors
//! (not a git repo, detached HEAD, I/O failures) collapse to `None`.

use std::path::{Path, PathBuf};

/// Open the nearest git repository containing `path` and return:
/// - the short current branch name (`None` when HEAD is detached or on error)
/// - the absolute path to `.git/HEAD` used as the watch target
///
/// Both fields are `None` when `path` is not inside any git repository.
/// Opens the repository only once so callers that need both values pay
/// a single discovery cost. Holds the spawn gate: gix may run `git` to find
/// its installation config.
pub fn probe(path: &Path) -> (Option<String>, Option<PathBuf>) {
    let _gate = crate::spawn_gate::shared();
    #[allow(clippy::disallowed_methods)] // under the gate
    let Ok(repo) = gix::discover(path) else {
        return (None, None);
    };
    let branch = repo
        .head_name()
        .ok()
        .flatten()
        .map(|n| n.shorten().to_string());
    let head = repo.git_dir().join("HEAD");
    (branch, Some(head))
}
