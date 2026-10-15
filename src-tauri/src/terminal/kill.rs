//! Killing everything a terminal's shell started.
//!
//! **Unix: by session id.** `portable-pty` makes the shell a session leader
//! (`setsid` before exec), so every process started from the terminal — also
//! a background job that outlived the shell and was reparented — has
//! `getsid(pid) == shell pid`, unless it called `setsid` itself. Walking the
//! process tree from the shell would miss exactly those reparented jobs. A
//! kill first signals the leader's process group (the shell and its
//! foreground jobs), so even a failed listing still kills the shell, then
//! lists the pids — `/proc` on Linux, `proc_listallpids` on macOS, `/bin/ps`
//! on other Unixes, once per pass for every session killed together
//! ([`kill_all`]) — and signals the session's members. It blocks for a few
//! milliseconds: call it off the IPC thread.
//!
//! **Pid reuse.** The session id is the leader's pid, which the system may
//! hand to an unrelated process as soon as the leader is reaped. So the
//! terminal's waiter observes the exit *without reaping* ([`wait_exit`],
//! `waitid(WNOWAIT)`) and reaps only after [`TreeKiller::kill`] has finished:
//! while the zombie exists the pid, hence the session id, cannot be reissued
//! and every `kill` by it is safe.
//!
//! **Hang-up first.** Closing a terminal (not the waiter's cleanup after an
//! exit) first sends SIGHUP to the leader's process group
//! ([`TreeKiller::hang_up`]) and gives the shell a short grace to exit — it
//! saves its history and hangs up its own jobs — before the SIGKILL passes.
//!
//! **Windows:** the shell is attached to a kill-on-close Job Object through
//! `supervisor::ProcessSupervisor::attach_process`, so killing the
//! tree closes the job handle, which ends the shell and every descendant. A
//! job that could not be set up falls back to killing the shell alone; the
//! pseudo console's closing then ends what is attached to it.

#[cfg(unix)]
use std::collections::HashSet;
#[cfg(all(test, unix))]
use std::sync::atomic::Ordering;
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::{MutexGuard, PoisonError};

#[cfg(windows)]
use portable_pty::ChildKiller;

#[cfg(windows)]
use crate::supervisor::ProcessSupervisor;

/// Passes over the process table: a job may fork between listing and signal.
#[cfg(unix)]
const MAX_PASSES: usize = 8;

/// Kills the shell's whole process tree, once.
///
/// The stored session id is only valid while the leader is unreaped (see the
/// module docs): the terminal's waiter calls [`TreeKiller::kill`] after the
/// exit and reaps the leader afterwards; a later `close` or respawn then
/// signals nothing.
#[cfg(unix)]
pub struct TreeKiller {
    /// The shell's pid (= its session id), or `0` once killed. Held for the
    /// whole kill, so a racing caller returns only once the kill is done:
    /// the waiter must not reap before that.
    sid: Mutex<i32>,
    /// Signals sent by the kills of this session so far, for tests.
    #[cfg(test)]
    signals: std::sync::atomic::AtomicUsize,
    /// Test hook: while set, the waiter pauses before its cleanup.
    #[cfg(test)]
    hold_cleanup: std::sync::atomic::AtomicBool,
}

#[cfg(unix)]
impl TreeKiller {
    /// `shell_pid` is the pid of the session leader `portable-pty` spawned.
    pub fn new(shell_pid: Option<u32>) -> Self {
        // Session ids 0 and 1 are never ours; signalling by them would hit
        // far more than this terminal.
        let sid = shell_pid
            .and_then(|pid| i32::try_from(pid).ok())
            .filter(|&pid| pid > 1)
            .unwrap_or(0);
        Self {
            sid: Mutex::new(sid),
            #[cfg(test)]
            signals: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            hold_cleanup: std::sync::atomic::AtomicBool::new(false),
        }
    }

    fn sid_lock(&self) -> MutexGuard<'_, i32> {
        self.sid.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hang the terminal up: SIGHUP the session leader's process group (the
    /// shell, a group leader, and whatever runs in its group), as a closing
    /// terminal emulator does, so the shell can save its history and hang up
    /// its jobs before [`TreeKiller::kill`]. Safe for the same reason as
    /// `kill`: while the id is stored the leader is unreaped, so no other
    /// group can carry its id. Returns whether a signal went out (`false`
    /// once killed, or without a session id).
    pub fn hang_up(&self) -> bool {
        let sid = self.sid_lock();
        if *sid == 0 {
            return false;
        }
        // SAFETY: plain-integer FFI call, no pointers involved. `*sid > 1`
        // is the unreaped leader's pid, so `-*sid` names only its process
        // group; a failure (`ESRCH`, the group is empty) is not actionable.
        unsafe { libc::kill(-*sid, libc::SIGHUP) };
        #[cfg(test)]
        self.signals.fetch_add(1, Ordering::AcqRel);
        drop(sid);
        true
    }

    /// SIGKILL every process of the session and forget its id. Later calls
    /// do nothing, but return only after an earlier call has finished.
    pub fn kill(&self) {
        kill_all(&[self]);
    }

    /// Forget the session id without signalling: for a leader that is gone
    /// without being held (its pid may already belong to someone else).
    pub fn forget(&self) {
        *self.sid_lock() = 0;
    }

    /// The stored session id, `0` once killed.
    #[cfg(test)]
    pub fn sid(&self) -> i32 {
        *self.sid_lock()
    }

    /// How many signals were sent so far.
    #[cfg(test)]
    pub fn signals(&self) -> usize {
        self.signals.load(Ordering::Acquire)
    }

    /// Test hook: pause (`true`) or release (`false`) the waiter's cleanup,
    /// which runs after the exit event and the grace period.
    #[cfg(test)]
    pub fn hold_cleanup(&self, hold: bool) {
        self.hold_cleanup.store(hold, Ordering::Release);
    }

    /// The waiter's test pause point, right before its cleanup.
    #[cfg(test)]
    pub fn pause_point(&self) {
        while self.hold_cleanup.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}

/// [`TreeKiller::kill`] for every one of `killers`, sharing each pass over
/// the process table. Every session id stays locked for the whole kill.
/// Only `TerminalManager::shutdown_all` passes several, all of shells it
/// took out of the map (so no other caller ever locks two of them, and the
/// lock order cannot deadlock); each must appear once.
#[cfg(unix)]
pub fn kill_all(killers: &[&TreeKiller]) {
    let mut held: Vec<MutexGuard<'_, i32>> = killers.iter().map(|k| k.sid_lock()).collect();
    let sids: Vec<i32> = held
        .iter()
        .map(|sid| **sid)
        .filter(|&sid| sid != 0)
        .collect();
    if sids.is_empty() {
        return;
    }
    #[cfg(not(test))]
    kill_sessions(&sids);
    #[cfg(test)]
    {
        // With several sessions, each is charged every signal of the kill.
        let signals = kill_sessions(&sids);
        for (killer, sid) in killers.iter().zip(&held) {
            if **sid != 0 {
                killer.signals.fetch_add(signals, Ordering::AcqRel);
            }
        }
    }
    for sid in &mut held {
        **sid = 0;
    }
}

/// Block until the process `pid` (a child of this process) has exited, but
/// leave it as a zombie: `waitid(WNOWAIT)` observes the exit without reaping,
/// so the pid stays reserved until the caller reaps it (`Child::wait`).
/// `Ok(code)` is the exit code, `None` for a death by signal; `Err` when the
/// wait failed (the child is not ours, or already reaped).
#[cfg(unix)]
pub fn wait_exit(pid: u32) -> std::io::Result<Option<i32>> {
    loop {
        // SAFETY: `siginfo_t` is a plain C struct (integers and a union of
        // them) for which all-zero bytes are a valid value.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a valid, exclusively borrowed `siginfo_t` that
        // `waitid` fills in; the other arguments are plain integers.
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                libc::id_t::from(pid),
                &raw mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if rc == 0 {
            // SAFETY: `waitid` succeeded for `WEXITED`, so it filled in
            // `si_code` and the child-status member that `si_status` reads.
            return Ok(exit_code(info.si_code, unsafe { info.si_status() }));
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// The exit code a `waitid` result stands for: only a normal exit has one.
#[cfg(unix)]
pub const fn exit_code(si_code: i32, si_status: i32) -> Option<i32> {
    if si_code == libc::CLD_EXITED {
        Some(si_status)
    } else {
        None // CLD_KILLED / CLD_DUMPED: died by signal `si_status`
    }
}

/// For tests: whether process `pid` exists (a zombie counts), by the null
/// signal.
#[cfg(all(test, unix))]
pub fn process_exists(pid: i32) -> bool {
    // SAFETY: plain-integer FFI call; signal 0 delivers nothing, it only
    // probes existence. `pid > 0` names one process, never a group.
    pid > 0 && unsafe { libc::kill(pid, 0) == 0 }
}

/// SIGKILL every process whose session id is one of `sids` (each > 1, an
/// unreaped leader's pid): first each leader's process group, then, until a
/// pass over the process table finds none that was not signalled before,
/// every listed member. Zombies are signalled and ignored harmlessly; a
/// listed pid is signalled once. Returns how many signals went out.
#[cfg(unix)]
fn kill_sessions(sids: &[i32]) -> usize {
    let mut killed: HashSet<i32> = HashSet::with_capacity(sids.len());
    for &sid in sids {
        // SAFETY: plain-integer FFI call, no pointers involved. `sid > 1` is
        // an unreaped leader's pid, so `-sid` names only its process group;
        // a failure (`ESRCH`, the group is empty) is not actionable.
        unsafe { libc::kill(-sid, libc::SIGKILL) };
        killed.insert(sid);
    }
    for _ in 0..MAX_PASSES {
        let members: Vec<i32> = list_pids()
            .into_iter()
            .filter(|&pid| {
                pid > 1
                    && !killed.contains(&pid)
                    && session_of(pid).is_some_and(|sid| sids.contains(&sid))
            })
            .collect();
        if members.is_empty() {
            break;
        }
        for &pid in &members {
            // SAFETY: plain-integer FFI call, no pointers involved. `pid > 1`
            // and its session is one of `sids`, so only processes of these
            // terminals are signalled; a failure (`ESRCH`, it just exited)
            // is not actionable.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        killed.extend(members);
    }
    killed.len()
}

#[cfg(unix)]
fn session_of(pid: i32) -> Option<i32> {
    // SAFETY: plain-integer FFI call, no pointers involved.
    let sid = unsafe { libc::getsid(pid) };
    (sid >= 0).then_some(sid)
}

#[cfg(target_os = "linux")]
fn list_pids() -> Vec<i32> {
    std::fs::read_dir("/proc").map_or_else(
        |_| Vec::new(),
        |dir| {
            dir.filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
                .collect()
        },
    )
}

/// Every pid, in-process (no `ps` fork). Empty on failure.
#[cfg(target_os = "macos")]
fn list_pids() -> Vec<i32> {
    /// Room for processes started between counting and listing; one started
    /// later is found by the next pass.
    const HEADROOM: usize = 64;
    // SAFETY: a null buffer of size 0 only asks for the number of pids.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    let mut pids: Vec<libc::pid_t> = vec![0; count + HEADROOM];
    let Ok(bytes) = libc::c_int::try_from(std::mem::size_of_val(pids.as_slice())) else {
        return Vec::new();
    };
    // SAFETY: `pids` is an exclusively borrowed buffer of exactly `bytes`
    // bytes of `pid_t`, which `proc_listallpids` fills with at most that
    // many bytes; it returns the number of pids written (or -1).
    let listed = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(listed).unwrap_or(0));
    pids
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn list_pids() -> Vec<i32> {
    use std::process::{Command, Stdio};
    Command::new("/bin/ps")
        .args(["-A", "-o", "pid="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_or_else(
            |_| Vec::new(),
            |out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .filter_map(|line| line.trim().parse().ok())
                    .collect()
            },
        )
}

/// Kills the shell and, through its job object, everything it started.
#[cfg(windows)]
pub struct TreeKiller {
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// The kill-on-close job (nothing to close if it could not be set up).
    job: ProcessSupervisor,
}

#[cfg(windows)]
impl TreeKiller {
    /// `process` is the shell's live process handle; the job is created and
    /// the shell assigned to it right away.
    pub fn new(
        killer: Box<dyn ChildKiller + Send + Sync>,
        process: Option<std::os::windows::io::RawHandle>,
    ) -> Self {
        Self {
            killer: Mutex::new(killer),
            job: process.map_or_else(
                ProcessSupervisor::unsupervised,
                ProcessSupervisor::attach_process,
            ),
        }
    }

    /// Close the job (killing the whole tree), then kill the shell directly
    /// in case the job could not be set up. Later calls do nothing harmful.
    pub fn kill(&self) {
        self.job.kill_tree();
        // Failing means the shell is gone already.
        let _ = self
            .killer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .kill();
    }
}

/// [`TreeKiller::kill`] for every one of `killers` (each job is closed on
/// its own; there is no process table to share).
#[cfg(windows)]
pub fn kill_all(killers: &[&TreeKiller]) {
    for killer in killers {
        killer.kill();
    }
}
