//! The gate every child process of this app is started through, so that no
//! child but Chromium can inherit Chromium's CDP pipe ends.
//!
//! Windows: `CreateProcess` with `bInheritHandles` — what
//! `std::process::Command` always passes — hands the new process *every*
//! inheritable handle of ours, not only the ones meant for it. Chromium
//! finds its pipe ends by handle value (`--remote-debugging-io-pipes`,
//! `browser/process/pipe.rs`), so they must be inheritable while its process
//! is created, and any other child created in that moment would get a copy:
//! a way into the browser past the broker. `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//! does not close that: it narrows only the `CreateProcess` call it is passed
//! to, so every *other* spawn would need one, std offers attribute lists
//! (`CommandExt::spawn_with_attributes`) on nightly only, and crates that
//! spawn on their own (gix) would still pass none. So the gate is a lock:
//! every spawn holds it shared ([`spawn`], [`output`], [`status`], or
//! [`shared`] around a spawn made for us), and Chromium's launch holds it
//! exclusively from making its ends inheritable until it has closed them
//! ([`exclusive`]). Spawns still run side by side; only Chromium's waits.
//!
//! Spawns made for us by code we call happen while the caller holds
//! [`shared`]: gix (the `git` it runs once to find its installation config,
//! filter drivers — every `gix::discover` and the git browser's reads),
//! `portable-pty` (the terminal's shell; its Windows `CreateProcess` inherits
//! nothing anyway) and `open::that_detached`. Every way to start a process
//! from this crate is a `disallowed-methods` entry in `clippy.toml` pointing
//! here, so a new spawn site cannot miss the gate.
//!
//! Unix: no handle is inheritable — Chromium's ends are moved to fds 3/4 in
//! its forked child, everything else is close-on-exec — so the gate does
//! nothing and its guards are empty.

use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
#[cfg(windows)]
use std::{
    cell::Cell,
    sync::{PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard},
};

/// Guards no data: poisoning means nothing and is ignored.
#[cfg(windows)]
static GATE: RwLock<()> = RwLock::new(());

#[cfg(windows)]
thread_local! {
    /// Whether this thread holds the gate: a nested [`shared`] must not take
    /// the read lock again, which deadlocks once a writer queued meanwhile.
    static HELD: Cell<bool> = const { Cell::new(false) };
}

/// The gate held shared: this thread may start children while it lives.
#[must_use = "the gate is held only while the guard lives"]
pub struct Shared {
    /// `None` when this thread held the gate already.
    #[cfg(windows)]
    lock: Option<RwLockReadGuard<'static, ()>>,
}

/// The gate held exclusively: no other thread starts a child while it lives.
#[must_use = "the gate is held only while the guard lives"]
pub struct Exclusive {
    #[cfg(windows)]
    _lock: RwLockWriteGuard<'static, ()>,
}

/// Hold the gate shared, around a spawn made by code we call. Nests: a
/// thread that holds the gate already gets an empty guard. Never wait,
/// holding it, for another thread that takes it: once an exclusive holder
/// queues, new shared holds wait for it, and it waits for this one.
#[cfg(windows)]
pub fn shared() -> Shared {
    let lock = (!HELD.get()).then(|| {
        let lock = GATE.read().unwrap_or_else(PoisonError::into_inner);
        HELD.set(true);
        lock
    });
    Shared { lock }
}

#[cfg(not(windows))]
pub const fn shared() -> Shared {
    Shared {}
}

#[cfg(windows)]
impl Drop for Shared {
    fn drop(&mut self) {
        if self.lock.is_some() {
            HELD.set(false);
        }
    }
}

/// Hold the gate exclusively: for Chromium's launch only, while its pipe
/// ends are inheritable. Waits for every spawn in flight. Never call it on
/// a thread that holds the gate.
#[cfg(windows)]
pub fn exclusive() -> Exclusive {
    debug_assert!(!HELD.get(), "the gate is held by this thread already");
    let lock = GATE.write().unwrap_or_else(PoisonError::into_inner);
    HELD.set(true);
    Exclusive { _lock: lock }
}

#[cfg(not(windows))]
pub const fn exclusive() -> Exclusive {
    Exclusive {}
}

#[cfg(windows)]
impl Drop for Exclusive {
    fn drop(&mut self) {
        HELD.set(false);
    }
}

/// `cmd.spawn()` through the gate.
#[allow(clippy::disallowed_methods)] // the gate is held
pub fn spawn(cmd: &mut Command) -> io::Result<Child> {
    let _gate = shared();
    cmd.spawn()
}

/// `cmd.output()` through the gate, held for the spawn only: stdin is null,
/// stdout and stderr are captured (set here, so a caller's own choice for
/// them is replaced).
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn(cmd)?.wait_with_output()
}

/// `cmd.status()` through the gate, held for the spawn only.
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    spawn(cmd)?.wait()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> Command {
        #[cfg(windows)]
        let mut cmd = Command::new("cmd");
        #[cfg(windows)]
        cmd.args(["/C", script]);
        #[cfg(unix)]
        let mut cmd = Command::new("sh");
        #[cfg(unix)]
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn output_captures_stdout_and_status_reports_the_exit_code() {
        let out = output(&mut shell("echo hi")).unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
        assert_eq!(status(&mut shell("exit 3")).unwrap().code(), Some(3));
    }

    #[test]
    fn a_missing_program_is_an_error() {
        let missing = std::env::temp_dir().join("omp-desktop-no-such-program");
        assert!(spawn(&mut Command::new(missing)).is_err());
    }

    /// While Chromium's ends are inheritable no other thread starts a child.
    #[cfg(windows)]
    #[test]
    fn an_exclusive_holder_keeps_every_spawn_out() {
        use std::sync::mpsc;
        use std::time::Duration;

        let gate = exclusive();
        let (tx, spawned) = mpsc::channel();
        let other = std::thread::spawn(move || {
            let _gate = shared();
            tx.send(()).unwrap();
        });
        assert_eq!(
            spawned.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "a spawn waits for the exclusive holder"
        );
        drop(gate);
        assert_eq!(spawned.recv_timeout(Duration::from_secs(5)), Ok(()));
        other.join().unwrap();
    }

    /// A thread holding the gate (a git browser read whose filter driver
    /// spawns) takes it again without waiting for an exclusive holder that
    /// queued meanwhile.
    #[cfg(windows)]
    #[test]
    fn a_nested_shared_hold_does_not_wait_for_a_queued_exclusive() {
        use std::sync::mpsc;
        use std::time::Duration;

        let outer = shared();
        let (queued_tx, exclusive_held) = mpsc::channel();
        let queued = std::thread::spawn(move || {
            let _gate = exclusive();
            queued_tx.send(()).unwrap();
        });
        std::thread::sleep(Duration::from_millis(100)); // the writer queues
        let inner = shared(); // returns: nested
        assert_eq!(
            exclusive_held.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the outer hold still keeps the exclusive holder out"
        );
        drop(inner);
        drop(outer);
        assert_eq!(exclusive_held.recv_timeout(Duration::from_secs(5)), Ok(()));
        queued.join().unwrap();
    }
}
