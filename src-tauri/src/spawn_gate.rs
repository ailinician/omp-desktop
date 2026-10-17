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
//! spawn on their own (gix) would still pass none.
//!
//! macOS: std's `io::pipe` opens the pipe and only then sets close-on-exec
//! on it, so a child forked in between keeps a copy of Chromium's ends.
//! Linux needs no gate (`pipe2(O_CLOEXEC)`, the ends moved to fds 3/4 in
//! Chromium's forked child), but it is one code path on every platform.
//!
//! So the gate is a lock: every spawn holds it shared ([`spawn`], [`output`],
//! [`status`], or [`shared`] around a spawn made for us), and Chromium's
//! launch holds it exclusively from creating its pipes until it has closed
//! its copies of Chromium's ends ([`exclusive`]). It prefers the shared side:
//! a launch waiting for spawns in flight does not hold up new ones (an omp
//! start on the IPC thread must not wait for a long git browser read), only
//! the launch itself, a few milliseconds, does. Spawns run side by side, and a
//! thread holding it shared can take it again (nested calls) without waiting.
//! The price: a launch waits until no spawn or gix read is in flight at all.
//!
//! Spawns made for us by code we call happen while the caller holds
//! [`shared`]: gix (the `git` it runs once to find its installation config,
//! filter drivers — every `gix::discover` and the git browser's reads),
//! `portable-pty` (the terminal's shell; its Windows `CreateProcess` inherits
//! nothing anyway) and `open::that_detached`. Every way to start a process
//! from this crate is a `disallowed-methods` entry in `clippy.toml` pointing
//! here, so the local clippy check (`AGENTS.md`'s verify command; CI does
//! not run clippy) rejects a new spawn site that misses the gate.

use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

struct State {
    /// Shared holds alive (on any thread).
    readers: usize,
    exclusive: bool,
}

struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

static GATE: Gate = Gate {
    state: Mutex::new(State {
        readers: 0,
        exclusive: false,
    }),
    changed: Condvar::new(),
};

/// The state, poisoning ignored: no code that can panic runs under the lock.
fn state() -> MutexGuard<'static, State> {
    GATE.state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn wait_while(
    state: MutexGuard<'static, State>,
    blocked: impl FnMut(&mut State) -> bool,
) -> MutexGuard<'static, State> {
    GATE.changed
        .wait_while(state, blocked)
        .unwrap_or_else(PoisonError::into_inner)
}

/// The gate held shared: this thread may start children while it lives.
#[must_use = "the gate is held only while the guard lives"]
pub struct Shared(());

/// The gate held exclusively: no other thread starts a child while it lives.
#[must_use = "the gate is held only while the guard lives"]
pub struct Exclusive(());

/// Hold the gate shared, around a spawn made by code we call. Waits only
/// while an exclusive hold is live, never for one that is waiting, so it
/// nests: a thread holding the gate shared takes it again at once.
pub fn shared() -> Shared {
    wait_while(state(), |s| s.exclusive).readers += 1;
    Shared(())
}

impl Drop for Shared {
    fn drop(&mut self) {
        let mut state = state();
        state.readers -= 1;
        if state.readers == 0 {
            GATE.changed.notify_all();
        }
    }
}

/// Hold the gate exclusively: for Chromium's launch only, while its pipe
/// ends exist in a form a child could inherit. Waits for every spawn in
/// flight. Never call it on a thread that holds the gate: it would wait for
/// itself.
pub fn exclusive() -> Exclusive {
    wait_while(state(), |s| s.exclusive || s.readers > 0).exclusive = true;
    Exclusive(())
}

impl Drop for Exclusive {
    fn drop(&mut self) {
        state().exclusive = false;
        GATE.changed.notify_all();
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
    use std::sync::mpsc;
    use std::time::Duration;

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

    /// An exclusive hold taken on a thread of its own; the receiver says
    /// when it is held, and dropping the sender releases it.
    fn exclusive_on_a_thread() -> (
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        std::thread::JoinHandle<()>,
    ) {
        let (held_tx, held) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let _gate = exclusive();
            held_tx.send(()).unwrap();
            let _ = released.recv();
        });
        (held, release, thread)
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
    #[test]
    fn an_exclusive_holder_keeps_every_spawn_out() {
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

    /// A launch waiting for a long read does not hold up other spawns, on
    /// this thread (nested) or any other; it gets the gate once they end.
    #[test]
    fn a_waiting_exclusive_holds_up_no_shared_hold() {
        let outer = shared();
        let (held, release, launch) = exclusive_on_a_thread();
        std::thread::sleep(Duration::from_millis(100)); // the launch waits
        let nested = shared();
        let (tx, spawned) = mpsc::channel();
        let other = std::thread::spawn(move || {
            let _gate = shared();
            tx.send(()).unwrap();
        });
        assert_eq!(
            spawned.recv_timeout(Duration::from_secs(5)),
            Ok(()),
            "a new spawn passes the waiting launch"
        );
        other.join().unwrap();
        assert_eq!(
            held.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the read in flight still keeps the launch out"
        );
        drop(nested);
        drop(outer);
        assert_eq!(held.recv_timeout(Duration::from_secs(5)), Ok(()));
        drop(release);
        launch.join().unwrap();
    }
}
