//! PTY manager tests: they spawn a real `sh` on a pseudo-terminal and
//! observe it through a collecting [`Emit`].

use super::pty::{resolve_shell_with, DEFAULT_SHELL};
use super::{Emit, OpenArgs, TerminalManager};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// A plain interactive `sh`. `/bin/sh -l` would read the developer's
/// `~/.profile` (which may `exec fish`), so the tests run `sh` through a
/// wrapper whose name (not a POSIX shell's) gets no `-l -i` arguments.
static PLAIN_SHELL: LazyLock<String> = LazyLock::new(|| {
    let dir = std::env::temp_dir().join(format!("omp-terminal-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("plainsh");
    std::fs::write(&path, "#!/bin/sh\nexec /bin/sh -i\n").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
});
const SLOW: Duration = Duration::from_secs(20);
/// Held by the tests whose result depends on the machine being otherwise
/// quiet (the burst's event count) and by those that start dozens of shells
/// and process-table scans (the lifecycle races), so they never overlap.
static LOAD_SENSITIVE: Mutex<()> = Mutex::new(());

/// Every event the manager emitted, in order.
#[derive(Default)]
struct Collector {
    data: Mutex<Vec<Vec<u8>>>,
    exits: Mutex<Vec<Option<i32>>>,
    /// The output emitted by the time the exit event arrived.
    text_at_exit: Mutex<Option<String>>,
}

impl Emit for Collector {
    fn data(&self, _id: &str, bytes: &[u8]) {
        self.data.lock().unwrap().push(bytes.to_vec());
    }

    fn exit(&self, _id: &str, code: Option<i32>) {
        *self.text_at_exit.lock().unwrap() = Some(self.text());
        self.exits.lock().unwrap().push(code);
    }
}

impl Collector {
    fn text(&self) -> String {
        let data = self.data.lock().unwrap();
        String::from_utf8_lossy(&data.concat()).into_owned()
    }

    fn total_bytes(&self) -> usize {
        self.data.lock().unwrap().iter().map(Vec::len).sum()
    }

    fn events(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    fn largest_event(&self) -> usize {
        self.data
            .lock()
            .unwrap()
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
    }

    fn exited(&self) -> bool {
        !self.exits.lock().unwrap().is_empty()
    }

    fn exit_codes(&self) -> Vec<Option<i32>> {
        self.exits.lock().unwrap().clone()
    }

    /// The decimal number right after the first `marker` in the output that
    /// is followed by a digit (the shell's echo of the typed command has a
    /// `$` there instead).
    fn number_after(&self, marker: &str) -> Option<i32> {
        let text = self.text();
        text.match_indices(marker).find_map(|(at, _)| {
            let digits: String = text[at + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn args(session_id: &str) -> OpenArgs {
    OpenArgs {
        session_id: session_id.to_owned(),
        cwd: std::env::temp_dir().to_string_lossy().into_owned(),
        // `OpenArgs` owns its strings; the static is shared by every test.
        shell: Some(PLAIN_SHELL.clone()),
        cols: 80,
        rows: 24,
    }
}

fn process_alive(pid: i32) -> bool {
    // SAFETY: plain-integer FFI call; signal 0 only probes existence.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn open(id: &str) -> (TerminalManager, Arc<Collector>) {
    let manager = TerminalManager::new();
    let collector = Arc::new(Collector::default());
    manager
        .open(Arc::clone(&collector), &args(id))
        .expect("open terminal");
    (manager, collector)
}

#[test]
fn shell_output_and_exit_code_are_emitted() {
    let (manager, out) = open("echo");
    // The quotes keep the echoed input from containing the output.
    manager.write("echo", "echo hel\"\"lo; exit 3\n").unwrap();

    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert!(out.text().contains("hello"), "output: {:?}", out.text());
    assert_eq!(out.exit_codes(), [Some(3)]);
}

#[test]
fn shell_gets_project_cwd_and_terminal_env() {
    let (manager, out) = open("env");
    manager
        .write(
            "env",
            "echo T=$TERM C=$COLORTERM O=$OMP_DESKTOP S=$OMP_DESKTOP_SESSION; pwd -P\n",
        )
        .unwrap();

    let cwd = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let wanted = format!(
        "T=xterm-256color C=truecolor O=1 S=env\r\n{}",
        cwd.display()
    );
    assert!(
        wait_until(SLOW, || out.text().contains(&wanted)),
        "output: {:?}",
        out.text()
    );
}

#[test]
fn burst_output_is_coalesced_into_few_capped_events() {
    let _quiet = LOAD_SENSITIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (manager, out) = open("burst");
    manager
        .write("burst", "yes | head -c 2000000; exit\n")
        .unwrap();

    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert!(
        out.total_bytes() >= 2_000_000,
        "bytes: {}",
        out.total_bytes()
    );
    assert!(out.events() < 400, "events: {}", out.events());
    assert!(
        out.largest_event() <= 64 * 1024,
        "largest event: {}",
        out.largest_event()
    );
}

#[test]
fn close_kills_background_jobs() {
    let (manager, out) = open("jobs");
    manager.write("jobs", "sleep 1000 & echo JOB=$!\n").unwrap();
    assert!(
        wait_until(SLOW, || out.number_after("JOB=").is_some()),
        "output: {:?}",
        out.text()
    );
    let job = out.number_after("JOB=").unwrap();
    assert!(process_alive(job), "background job did not start");

    manager.close("jobs");

    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(job)),
        "background job {job} survived close"
    );
    // Closing is silent: the tab is gone, nobody listens for the exit.
    std::thread::sleep(Duration::from_millis(300));
    assert!(!out.exited());
}

#[test]
fn ctrl_c_reaches_the_foreground_process() {
    let (manager, out) = open("ctrlc");
    // The marker is printed (not just echoed: the quotes split it in the
    // typed line) once the foreground process runs, then it becomes `sleep`.
    manager
        .write("ctrlc", "sh -c 'echo RUN\"\"NING; exec sleep 30'\n")
        .unwrap();
    assert!(wait_until(SLOW, || out.text().contains("RUNNING")));
    std::thread::sleep(Duration::from_millis(100));

    manager.write("ctrlc", "\x03").unwrap();
    manager.write("ctrlc", "echo DONE$?\n").unwrap();

    // 130 = killed by SIGINT.
    assert!(
        wait_until(Duration::from_secs(2), || out.text().contains("DONE130")),
        "output: {:?}",
        out.text()
    );
}

#[test]
fn open_is_idempotent_while_alive_and_respawns_after_exit() {
    let (manager, out) = open("again");
    manager.write("again", "echo A=$$\n").unwrap();
    assert!(wait_until(SLOW, || out.number_after("A=").is_some()));
    let first = out.number_after("A=").unwrap();

    manager.open(Arc::clone(&out), &args("again")).unwrap();
    manager.write("again", "echo B=$$\n").unwrap();
    assert!(wait_until(SLOW, || out.number_after("B=").is_some()));
    assert_eq!(out.number_after("B="), Some(first), "second open respawned");

    manager.write("again", "exit\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()));

    manager.open(Arc::clone(&out), &args("again")).unwrap();
    manager.write("again", "echo C=$$\n").unwrap();
    assert!(wait_until(SLOW, || out.number_after("C=").is_some()));
    assert_ne!(out.number_after("C="), Some(first), "no new shell");
}

#[test]
fn resize_changes_the_pty_size() {
    let (manager, out) = open("size");
    manager.resize("size", 100, 40).unwrap();
    manager.write("size", "stty size\n").unwrap();

    assert!(
        wait_until(SLOW, || out.text().contains("40 100\r\n")),
        "output: {:?}",
        out.text()
    );
}

#[test]
fn shutdown_all_kills_every_shell() {
    let (manager, out) = open("down");
    manager.write("down", "echo SH=$$\n").unwrap();
    assert!(wait_until(SLOW, || out.number_after("SH=").is_some()));
    let shell = out.number_after("SH=").unwrap();

    manager.shutdown_all();

    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(shell)),
        "shell {shell} survived shutdown_all"
    );
    assert!(manager.write("down", "x").is_err(), "entry not removed");
}

#[test]
fn bad_requests_are_rejected() {
    let manager = TerminalManager::new();
    let out = Arc::new(Collector::default());
    let open = |edit: &dyn Fn(&mut OpenArgs)| {
        let mut bad = args("bad");
        edit(&mut bad);
        manager.open(Arc::clone(&out), &bad)
    };

    assert!(open(&|a| a.cwd = "/definitely/not/a/dir".to_owned()).is_err());
    assert!(open(&|a| a.cols = 0).is_err());
    assert!(open(&|a| a.rows = 0).is_err());
    assert!(open(&|a| a.session_id.clear()).is_err());
    assert!(open(&|a| a.shell = Some("relative/sh".to_owned())).is_err());
    assert!(manager.write("none", "x").is_err());
    assert!(manager.resize("none", 80, 24).is_err());
    manager.close("none"); // closing an unknown session is a no-op
}

#[test]
fn a_background_job_dies_with_its_exited_shell_without_any_close() {
    let (manager, out) = open("orphan");
    manager
        .write("orphan", "sleep 1000 & echo JOB=$!\n")
        .unwrap();
    assert!(wait_until(SLOW, || out.number_after("JOB=").is_some()));
    let pty = manager.get("orphan").unwrap();
    manager.write("orphan", "exit\n").unwrap();
    assert!(
        wait_until(SLOW, || out.exited()),
        "no exit event; output: {:?}",
        out.text()
    );
    let job = out.number_after("JOB=").expect("no job pid");

    // Nobody closes or replaces the terminal: the exit alone ends the job.
    assert!(
        wait_until(Duration::from_secs(3), || !process_alive(job)),
        "job {job} outlived its shell"
    );
    // Counted just after the signal went out.
    assert!(
        wait_until(SLOW, || pty.killer().signals() >= 1),
        "the job was not signalled"
    );
    assert_eq!(pty.killer().sid(), 0, "session id kept after the exit");
}

/// The reaped shell's pid can be reused by the system: nothing may signal
/// that session id any more once the shell is gone — not a later `close`,
/// not the respawn replacing the entry.
#[test]
fn an_exited_shell_is_never_signalled_again_by_close_or_respawn() {
    let (manager, out) = open("reuse");
    let pty = manager.get("reuse").unwrap();
    assert!(pty.killer().sid() > 1, "no session id while the shell runs");
    manager.write("reuse", "exit\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert!(
        wait_until(SLOW, || pty.killer().sid() == 0),
        "session id kept after the exit"
    );
    let signals = pty.killer().signals();

    // Respawn replaces (and kills) the exited entry; then close kills the
    // new one. Neither touches the old session id.
    manager.open(Arc::clone(&out), &args("reuse")).unwrap();
    manager.close("reuse");

    assert_eq!(
        pty.killer().signals(),
        signals,
        "signals sent after the shell was gone"
    );
}

/// `ps` state of `pid` (`Z...` for a zombie), empty when it is gone.
fn process_state(pid: i32) -> String {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("run ps");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Releases the paused cleanup of a terminal when dropped, so a failing
/// assertion never leaves its waiter thread paused.
struct ResumeCleanup(Arc<super::pty::Pty>);

impl Drop for ResumeCleanup {
    fn drop(&mut self) {
        self.0.killer().hold_cleanup(false);
    }
}

/// The exit code comes from `waitid` (the shell is not reaped to read it):
/// a normal exit gives its code, a death by signal none.
#[test]
fn the_exit_code_is_read_without_reaping_the_shell() {
    let (manager, out) = open("code7");
    manager.write("code7", "exit 7\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert_eq!(out.exit_codes(), [Some(7)]);

    let (manager, out) = open("signalled");
    manager.write("signalled", "kill -9 $$\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert_eq!(out.exit_codes(), [None], "a signal has no exit code");
    drop(manager);

    assert_eq!(super::kill::exit_code(libc::CLD_EXITED, 7), Some(7));
    assert_eq!(super::kill::exit_code(libc::CLD_KILLED, 9), None);
    assert_eq!(super::kill::exit_code(libc::CLD_DUMPED, 11), None);
}

/// While the session is not cleaned up the exited shell stays an unreaped
/// zombie, so its pid — the session id — cannot be reissued to someone else
/// and a kill by it cannot hit a stranger. Reaped only after the cleanup.
#[test]
fn the_exited_shell_stays_a_zombie_until_its_session_is_cleaned_up() {
    let (manager, out) = open("zombie");
    let pty = manager.get("zombie").unwrap();
    let pid = pty.killer().sid();
    assert!(pid > 1);
    pty.killer().hold_cleanup(true);
    let _resume = ResumeCleanup(Arc::clone(&pty));

    manager.write("zombie", "exit 7\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    assert_eq!(out.exit_codes(), [Some(7)]);
    // Well past the grace period: the cleanup is the only thing pending.
    std::thread::sleep(super::pump::READER_GRACE * 2);

    assert!(process_alive(pid), "the leader's pid was released early");
    assert!(
        process_state(pid).starts_with('Z'),
        "leader {pid} is not a zombie: {:?}",
        process_state(pid)
    );
    assert_eq!(pty.killer().sid(), pid, "session id dropped before cleanup");

    pty.killer().hold_cleanup(false);
    assert!(
        wait_until(SLOW, || process_state(pid).is_empty()),
        "the zombie {pid} was never reaped"
    );
    assert!(!process_alive(pid));
    assert_eq!(pty.killer().sid(), 0, "session id kept after the cleanup");
}

/// A `close` during the grace period kills by the still-reserved session id,
/// but leaves the reaping to the waiter: the zombie survives the `close` and
/// is reaped exactly once, after the cleanup.
#[test]
fn a_close_during_the_grace_period_leaves_the_reaping_to_the_waiter() {
    let (manager, out) = open("zombie-close");
    let pty = manager.get("zombie-close").unwrap();
    let pid = pty.killer().sid();
    pty.killer().hold_cleanup(true);
    let _resume = ResumeCleanup(Arc::clone(&pty));

    manager.write("zombie-close", "exit\n").unwrap();
    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    manager.close("zombie-close");

    assert_eq!(pty.killer().sid(), 0, "close did not clear the session id");
    assert!(
        process_state(pid).starts_with('Z'),
        "leader {pid} reaped by the close: {:?}",
        process_state(pid)
    );

    pty.killer().hold_cleanup(false);
    assert!(
        wait_until(SLOW, || process_state(pid).is_empty()),
        "the zombie {pid} was never reaped"
    );
}

/// A shell that reads no input (raw tty, no echo) and sleeps, so whatever is
/// written to it piles up in the pty.
fn stalled_shell(id: &str) -> (TerminalManager, Arc<Collector>) {
    let (manager, out) = open(id);
    manager.write(id, "stty raw -echo; sleep 5\n").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    (manager, out)
}

#[test]
fn write_returns_at_once_when_the_shell_reads_nothing() {
    let (manager, _out) = stalled_shell("stalled");

    let started = Instant::now();
    manager.write("stalled", "x".repeat(1 << 20)).unwrap();
    let took = started.elapsed();

    assert!(took < Duration::from_millis(100), "write took {took:?}");
}

#[test]
fn write_backlog_is_rejected_instead_of_blocking() {
    let (manager, _out) = stalled_shell("backlog");

    let chunk = "x".repeat(64 * 1024);
    let started = Instant::now();
    let error = (0..5000).find_map(|_| manager.write("backlog", chunk.clone()).err());

    assert_eq!(error.as_deref(), Some("terminal input backlog"));
    assert!(started.elapsed() < Duration::from_secs(5), "writes blocked");
}

#[test]
fn all_output_is_out_before_the_exit_event() {
    let (manager, out) = open("drain");
    // More than the pump emits in one go, so the tail is still queued
    // when the shell exits.
    manager
        .write(
            "drain",
            "echo A\"\"1; head -c 1500000 /dev/zero | tr '\\0' z; echo END\"\"9; exit 0\n",
        )
        .unwrap();

    assert!(wait_until(SLOW, || out.exited()), "no exit event");
    let seen = out.text_at_exit.lock().unwrap().clone().unwrap();
    assert!(seen.contains("A1"), "before exit: {seen:.200?}");
    assert!(
        seen.contains("END9"),
        "tail lost: {:?}",
        &seen[seen.len() - 100..]
    );
    assert_eq!(out.exit_codes(), [Some(0)]);
}

/// When its session leader exits, the pty hangs up (observed on macOS): a
/// background job holding the slave open can neither keep the reader from
/// EOF nor write to the terminal any more. The exit event follows at once
/// and the job's late output is dropped, not forwarded after the exit.
#[cfg(target_os = "macos")]
#[test]
fn a_job_does_not_delay_the_exit_and_its_late_output_is_dropped() {
    let (manager, out) = open("holdout");
    manager
        .write(
            "holdout",
            "exec 3>&1; (sleep 0.1; echo B\"\"2 >&3) & exit 0\n",
        )
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(2), || out.exited()),
        "exit event waited for the job"
    );
    std::thread::sleep(Duration::from_millis(600));
    assert!(!out.text().contains("B2"), "output after the exit event");
    assert_eq!(out.exit_codes(), [Some(0)]);
}

/// On Linux a background job may keep the slave open after the shell exited:
/// output it produces within the reader's grace period (250 ms) is still
/// delivered, and before the exit event, which then follows promptly.
#[cfg(target_os = "linux")]
#[test]
fn a_jobs_output_within_the_grace_period_is_delivered_before_the_exit() {
    let (manager, out) = open("holdout");
    manager
        .write(
            "holdout",
            "exec 3>&1; (sleep 0.1; echo B\"\"2 >&3) & exit 0\n",
        )
        .unwrap();

    assert!(
        wait_until(Duration::from_secs(2), || out.exited()),
        "exit event waited for the job"
    );
    let seen = out.text_at_exit.lock().unwrap().clone().unwrap();
    assert!(seen.contains("B2"), "job output after the exit: {seen:?}");
    assert_eq!(out.exit_codes(), [Some(0)]);
}

/// A shell that records its pid in `pidfile` (appending) before it becomes
/// an interactive `sh`: lets a test see every shell a manager ever started.
fn recording_shell(tag: &str) -> (String, PathBuf) {
    let dir = std::env::temp_dir().join(format!("omp-terminal-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let pidfile = dir.join("pids");
    let _ = std::fs::remove_file(&pidfile);
    let path = dir.join("recordsh");
    let script = format!(
        "#!/bin/sh\necho $$ >> '{}'\nexec /bin/sh -i\n",
        pidfile.display()
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (path.to_string_lossy().into_owned(), pidfile)
}

fn recorded_pids(pidfile: &std::path::Path) -> Vec<i32> {
    std::fs::read_to_string(pidfile)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

/// Run `first` and `second` at the same moment on two threads.
fn concurrently(first: impl FnOnce() + Send, second: impl FnOnce() + Send) {
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            start.wait();
            first();
        });
        scope.spawn(|| {
            start.wait();
            second();
        });
    });
}

#[test]
fn open_racing_close_leaves_no_shell_and_no_entry_after_a_final_close() {
    let _quiet = LOAD_SENSITIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (shell, pidfile) = recording_shell("open-close");
    let manager = TerminalManager::new();
    let out = Arc::new(Collector::default());
    let open_args = OpenArgs {
        shell: Some(shell),
        ..args("race")
    };
    for round in 0..20 {
        concurrently(
            || {
                manager.open(Arc::clone(&out), &open_args).expect("open");
            },
            || manager.close("race"),
        );
        manager.close("race");

        assert!(manager.ptys().is_empty(), "round {round}: entry left");
        let pids = recorded_pids(&pidfile);
        assert!(
            wait_until(Duration::from_secs(2), || pids
                .iter()
                .all(|&p| !process_alive(p))),
            "round {round}: a shell survived: {pids:?}"
        );
    }
}

#[test]
fn overlapping_opens_start_one_shell() {
    let _quiet = LOAD_SENSITIVE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let (shell, pidfile) = recording_shell("open-open");
    let manager = TerminalManager::new();
    let out = Arc::new(Collector::default());
    let open_args = OpenArgs {
        shell: Some(shell),
        ..args("twice")
    };
    for round in 0..10 {
        concurrently(
            || {
                manager
                    .open(Arc::clone(&out), &open_args)
                    .expect("first open");
            },
            || {
                manager
                    .open(Arc::clone(&out), &open_args)
                    .expect("second open");
            },
        );
        let expected = round + 1;
        assert!(
            wait_until(SLOW, || recorded_pids(&pidfile).len() >= expected),
            "round {round}: shell never started"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            recorded_pids(&pidfile).len(),
            expected,
            "round {round}: a second shell was started"
        );
        manager.close("twice");
    }
}

#[test]
fn resolve_shell_prefers_setting_then_env_then_account_then_default() {
    let sh = PathBuf::from("/bin/sh");
    let login = vec!["-l".to_owned(), "-i".to_owned()];
    let missing = "/definitely/not/a/shell";
    let not_executable = "/etc/hosts";
    // A stand-in for the account's shell that exists on every unix runner.
    let dir = std::env::temp_dir().join(format!("omp-acct-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let zsh_path = dir.join("zsh");
    std::fs::write(&zsh_path, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&zsh_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let zsh = zsh_path.to_str().unwrap();
    // (preference, $SHELL, account shell, expected program); every case is a
    // POSIX shell, so every case gets `-l -i`.
    let ok = [
        (Some("/bin/sh"), Some(zsh), None, "/bin/sh"),
        (None, Some("/bin/sh"), Some(zsh), "/bin/sh"),
        (Some(""), Some("/bin/sh"), None, "/bin/sh"),
        (Some("  "), None, None, DEFAULT_SHELL),
        (None, Some(missing), None, DEFAULT_SHELL),
        (None, Some("sh"), None, DEFAULT_SHELL),
        (None, None, None, DEFAULT_SHELL),
        // $SHELL unset or unusable → the account's login shell.
        (None, None, Some(zsh), zsh),
        (None, Some(missing), Some(zsh), zsh),
        (None, None, Some(missing), DEFAULT_SHELL),
    ];
    for (pref, env, account, program) in ok {
        let (got, got_args) = resolve_shell_with(pref, env, account).unwrap();
        assert_eq!(
            got,
            PathBuf::from(program),
            "pref {pref:?} env {env:?} account {account:?}"
        );
        assert_eq!(
            got_args, login,
            "pref {pref:?} env {env:?} account {account:?}"
        );
    }
    assert_eq!(resolve_shell_with(None, None, None).unwrap().0, sh);

    for pref in [missing, not_executable, "sh", "./sh", "/bin"] {
        assert!(
            resolve_shell_with(Some(pref), Some("/bin/sh"), None).is_err(),
            "{pref} must be rejected"
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn account_shell_output_is_parsed() {
    use crate::terminal::pty::parse_account_shell;
    assert_eq!(
        parse_account_shell("UserShell: /opt/homebrew/bin/fish\n").as_deref(),
        Some("/opt/homebrew/bin/fish")
    );
    assert_eq!(
        parse_account_shell("me:x:1000:1000:Me,,,:/home/me:/usr/bin/zsh\n").as_deref(),
        Some("/usr/bin/zsh")
    );
    assert_eq!(parse_account_shell(""), None);
    assert_eq!(parse_account_shell("No such key: UserShell"), None);
}

#[cfg(target_os = "macos")]
#[test]
fn account_shell_lookup_finds_an_absolute_shell() {
    let shell = super::pty::account_shell().expect("dscl lookup");
    assert!(shell.starts_with('/'), "{shell}");
}

#[test]
fn only_posix_shells_get_login_interactive_args() {
    let dir = std::env::temp_dir().join(format!("omp-shell-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut seen = Vec::new();
    for name in ["bash", "zsh", "fish", "dash", "ksh", "sh", "nu", "pwsh"] {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        let (_, shell_args) = resolve_shell_with(path.to_str(), None, None).unwrap();
        seen.push((name, shell_args.len()));
    }
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(
        seen,
        [
            ("bash", 2),
            ("zsh", 2),
            ("fish", 2),
            ("dash", 2),
            ("ksh", 2),
            ("sh", 2),
            ("nu", 0),
            ("pwsh", 0),
        ]
    );
}
