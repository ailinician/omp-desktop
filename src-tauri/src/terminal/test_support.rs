//! Platform-neutral helpers for the terminal tests (`tests.rs` on unix,
//! `tests_windows.rs` on Windows, and the module tests).

use super::{Emit, TerminalManager};
use std::ffi::OsStr;
use std::sync::Arc;

pub use crate::test_support::wait_until;

/// `id` registered on `manager` with the temp dir as its folder, its shell
/// opened and observed by a fresh `C`.
pub fn open_with<C: Default>(manager: TerminalManager, id: &str) -> (TerminalManager, Arc<C>)
where
    Arc<C>: Emit,
{
    manager.register_tab(id, Some(std::env::temp_dir().as_path()));
    let out = Arc::new(C::default());
    manager
        .open(Arc::clone(&out), id, 80, 24)
        .expect("open terminal");
    (manager, out)
}

/// What `program` with `args` printed on stdout (lossy UTF-8).
pub fn stdout_of<I, S>(program: &str, args: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = std::process::Command::new(program)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    String::from_utf8_lossy(&out.stdout).into_owned()
}
