//! Killing everything a terminal's shell started.
//!
//! **Unix: by session id.** `portable-pty` makes the shell a session leader
//! (`setsid` before exec), so every process started from the terminal — also
//! a background job that outlived the shell and was reparented — has
//! `getsid(pid) == shell pid`, unless it called `setsid` itself. Walking the
//! process tree from the shell would miss exactly those reparented jobs. The
//! pids come from `/proc` (Linux) or one `ps` run (other Unixes), so killing
//! blocks for a few milliseconds: call it off the IPC thread.
//!
//! **Pid reuse.** The session id is the leader's pid, which the system may
//! hand to an unrelated process as soon as the leader is reaped. So the
//! terminal's waiter observes the exit *without reaping* ([`wait_exit`],
//! `waitid(WNOWAIT)`) and reaps only after [`TreeKiller::kill`] has finished:
//! while the zombie exists the pid, hence the session id, cannot be reissued
//! and every `kill` by it is safe.
//!
//! **Windows:** the shell is assigned to a terminal-owned Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (the pattern of
//! `agent/supervisor.rs`), so closing the job handle kills the shell and every
//! descendant. A job that could not be set up falls back to killing the shell
//! alone; the pseudo console's closing then ends what is attached to it.

#[cfg(windows)]
use std::sync::atomic::AtomicPtr;
#[cfg(any(windows, test))]
use std::sync::atomic::Ordering;
use std::sync::Mutex;
#[cfg(unix)]
use std::sync::PoisonError;

#[cfg(windows)]
use portable_pty::ChildKiller;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

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
    /// Signals sent so far, for tests.
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

    /// SIGKILL every process of the session and forget its id. Later calls
    /// do nothing, but return only after an earlier call has finished.
    pub fn kill(&self) {
        let mut sid = self.sid.lock().unwrap_or_else(PoisonError::into_inner);
        if *sid != 0 {
            #[cfg(test)]
            self.signals.fetch_add(kill_session(*sid), Ordering::AcqRel);
            #[cfg(not(test))]
            kill_session(*sid);
            *sid = 0;
        }
    }

    /// Forget the session id without signalling: for a leader that is gone
    /// without being held (its pid may already belong to someone else).
    pub fn forget(&self) {
        *self.sid.lock().unwrap_or_else(PoisonError::into_inner) = 0;
    }

    /// The stored session id, `0` once killed.
    #[cfg(test)]
    pub fn sid(&self) -> i32 {
        *self.sid.lock().unwrap_or_else(PoisonError::into_inner)
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

/// SIGKILL every process whose session id is `sid` (> 1), repeating until a
/// pass finds none that was not signalled before. Zombies are signalled and
/// ignored harmlessly; a pid is never signalled twice. Returns how many
/// processes were signalled.
#[cfg(unix)]
fn kill_session(sid: i32) -> usize {
    let mut killed: Vec<i32> = Vec::new();
    for _ in 0..MAX_PASSES {
        let members: Vec<i32> = list_pids()
            .into_iter()
            .filter(|&pid| pid > 1 && !killed.contains(&pid) && session_of(pid) == Some(sid))
            .collect();
        if members.is_empty() {
            break;
        }
        for &pid in &members {
            // SAFETY: plain-integer FFI call, no pointers involved. `pid > 1`
            // and its session is `sid > 1`, so only processes of this
            // terminal are signalled; a failure (`ESRCH`, it just exited) is
            // not actionable.
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

#[cfg(all(unix, not(target_os = "linux")))]
fn list_pids() -> Vec<i32> {
    use std::process::{Command, Stdio};
    Command::new("ps")
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

/// Kills the shell and, through the job object, everything it started.
#[cfg(windows)]
pub struct TreeKiller {
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Job object handle, or null once closed (or if setup failed).
    job: AtomicPtr<core::ffi::c_void>,
}

#[cfg(windows)]
impl TreeKiller {
    /// `process` is the shell's live process handle; the job is created and
    /// the shell assigned to it right away.
    pub fn new(killer: Box<dyn ChildKiller + Send + Sync>, process: Option<HANDLE>) -> Self {
        let job = process.map_or(std::ptr::null_mut(), create_job_for);
        Self {
            killer: Mutex::new(killer),
            job: AtomicPtr::new(job),
        }
    }

    /// Close the job (killing the whole tree), then kill the shell directly
    /// in case the job could not be set up. Later calls do nothing harmful.
    pub fn kill(&self) {
        let job = self.job.swap(std::ptr::null_mut(), Ordering::AcqRel);
        if !job.is_null() {
            // SAFETY: `job` was produced by `create_job_for` and not closed
            // before: the swap above guarantees exactly one caller sees the
            // non-null value. Closing the last handle to a job carrying
            // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` terminates every process
            // assigned to it.
            unsafe { CloseHandle(job) };
        }
        // Failing means the shell is gone already.
        let _ = self
            .killer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .kill();
    }
}

/// A kill-on-close job holding `process`, or null when any step fails (the
/// terminal then just kills the shell, as before the job existed).
#[cfg(windows)]
fn create_job_for(process: HANDLE) -> HANDLE {
    // SAFETY: null security attributes and a null (anonymous) name is the
    // documented way to create an unnamed job object usable only through the
    // returned handle, which this function owns until it returns it (closed
    // below on any setup failure, otherwise by `TreeKiller::kill`).
    let job: HANDLE = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return std::ptr::null_mut();
    }

    // Every other field defaulted (all-zero) means "no other limits".
    let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
        BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `job` is the handle just created above; `info` is a correctly
    // sized `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` (defaulted, with its one
    // relevant field set), matching what `SetInformationJobObject` expects
    // for `JobObjectExtendedLimitInformation`.
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(info).cast(),
            u32::try_from(std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                .unwrap_or_default(),
        )
    };
    if configured == 0 {
        // SAFETY: `job` was created by this function and has not been
        // assigned to anything or closed yet — sole owner.
        unsafe { CloseHandle(job) };
        return std::ptr::null_mut();
    }

    // SAFETY: `job` is a valid, configured job handle; `process` is the live
    // handle of the shell's `Child`, which outlives this call.
    let assigned = unsafe { AssignProcessToJobObject(job, process) };
    if assigned == 0 {
        // SAFETY: same reasoning as the `configured == 0` branch above.
        unsafe { CloseHandle(job) };
        return std::ptr::null_mut();
    }
    job
}
