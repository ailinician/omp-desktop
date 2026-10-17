//! The pipe pair a launched Chromium speaks CDP over
//! (`--remote-debugging-pipe`): Chromium reads commands from one and writes
//! replies and events to the other, and opens no debugging port. Only this
//! process and Chromium hold the pipe ends, so no other local process — of
//! this user or another, our own children included — can reach the browser
//! except through the broker, and the pipe ends with Chromium.
//!
//! Unix: Chromium reads fd 3 and writes fd 4; the child's ends are moved
//! there between fork and exec, and are close-on-exec everywhere else. On
//! macOS std opens a pipe and only then sets close-on-exec on it, so
//! [`attach`] creates them under the exclusive spawn gate
//! ([`crate::spawn_gate`]): no child is forked in between.
//! Windows: Chromium takes the two handle values from
//! `--remote-debugging-io-pipes=<read>,<write>`, so its ends must be
//! inheritable while its process is created — and every child created in
//! that moment inherits every inheritable handle. [`CdpPipes::spawn`] makes
//! them inheritable only under the exclusive spawn gate and closes them
//! before releasing it.

use std::io::{self, PipeReader, PipeWriter};
use std::process::{Child, Command};

use crate::spawn_gate;

/// Our ends of a launched Chromium's CDP pipes, and the ends its process
/// takes over, until [`spawn`](Self::spawn).
pub struct CdpPipes {
    from_chromium: PipeReader,
    to_chromium: PipeWriter,
    /// Chromium's ends: what it reads commands from, what it writes to.
    child: (PipeReader, PipeWriter),
}

impl CdpPipes {
    /// Start `cmd` (set up by [`attach`]) with Chromium's ends, then close
    /// our copies of them, so the pipes end when Chromium does, and hand
    /// over ours: what Chromium writes, what it reads. Waits for every other
    /// spawn in flight (the gate).
    #[allow(clippy::disallowed_methods)] // the gate is held exclusively
    pub fn spawn(self, cmd: &mut Command) -> io::Result<(Child, PipeReader, PipeWriter)> {
        let Self {
            from_chromium,
            to_chromium,
            child,
        } = self;
        let spawned = {
            let _gate = spawn_gate::exclusive();
            #[cfg(windows)]
            let spawned = make_inheritable(&child.0, &child.1).and_then(|()| cmd.spawn());
            #[cfg(unix)]
            let spawned = cmd.spawn();
            // Closed, inheritable as they are, before another spawn may run.
            drop(child);
            spawned
        };
        Ok((spawned?, from_chromium, to_chromium))
    }
}

/// Create the pipes and set `cmd` up to hand Chromium its ends.
pub fn attach(cmd: &mut Command) -> io::Result<CdpPipes> {
    let (child_reads, to_chromium, from_chromium, child_writes) = {
        // Not atomic on macOS: the ends are inheritable for a moment.
        #[cfg(target_vendor = "apple")]
        let _gate = spawn_gate::exclusive();
        let (child_reads, to_chromium) = io::pipe()?;
        let (from_chromium, child_writes) = io::pipe()?;
        (child_reads, to_chromium, from_chromium, child_writes)
    };
    cmd.arg("--remote-debugging-pipe");
    hand_over(cmd, &child_reads, &child_writes);
    Ok(CdpPipes {
        from_chromium,
        to_chromium,
        child: (child_reads, child_writes),
    })
}

#[cfg(unix)]
fn hand_over(cmd: &mut Command, reads: &PipeReader, writes: &PipeWriter) {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    // SAFETY: the closure runs in the forked child before exec and calls
    // only async-signal-safe functions (`fcntl`, `dup2`, `close`) on fds
    // that are open in the child: `reads`/`writes` stay open in this process
    // until `CdpPipes::spawn` has spawned. The copies above 9 keep `dup2`
    // from clobbering one end with the other when either already is fd 3 or
    // 4; `dup2` clears close-on-exec on 3 and 4, while the originals (opened
    // close-on-exec by `io::pipe`) and the closed copies do not reach
    // Chromium.
    unsafe {
        cmd.pre_exec(move || {
            let r = libc::fcntl(reads, libc::F_DUPFD, 10);
            let w = libc::fcntl(writes, libc::F_DUPFD, 10);
            if r < 0 || w < 0 || libc::dup2(r, 3) < 0 || libc::dup2(w, 4) < 0 {
                return Err(io::Error::last_os_error());
            }
            libc::close(r);
            libc::close(w);
            Ok(())
        });
    }
}

#[cfg(windows)]
fn hand_over(cmd: &mut Command, reads: &PipeReader, writes: &PipeWriter) {
    use std::os::windows::io::AsRawHandle;

    // An inherited handle keeps its value in the child. Handle values fit
    // in 32 bits (Chromium parses them as such).
    cmd.arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads.as_raw_handle() as usize,
        writes.as_raw_handle() as usize
    ));
}

/// Let Chromium inherit its ends. Only under the exclusive gate: from here
/// until they are closed, every child created inherits them.
#[cfg(windows)]
fn make_inheritable(reads: &PipeReader, writes: &PipeWriter) -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE, HANDLE_FLAG_INHERIT};

    for handle in [reads.as_raw_handle(), writes.as_raw_handle()] {
        // SAFETY: `handle` is a live pipe handle owned by `reads`/`writes`,
        // which outlive this call; only its inherit flag changes.
        let ok = unsafe {
            SetHandleInformation(handle as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A stand-in for Chromium that exits at once.
    fn exiting() -> Command {
        crate::test_support::shell("exit")
    }

    /// A child of ours that outlives the test's reads unless killed.
    fn bystander() -> Child {
        #[cfg(windows)]
        let mut cmd = Command::new("ping");
        #[cfg(windows)]
        cmd.args(["-n", "30", "127.0.0.1"]);
        #[cfg(unix)]
        let mut cmd = Command::new("sleep");
        #[cfg(unix)]
        cmd.arg("30");
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
        spawn_gate::spawn(&mut cmd).unwrap()
    }

    /// Read `from_chromium` to its end on a thread: whether that came
    /// within `limit` (no process but the exited stand-in held its far end).
    fn ends_within(mut from_chromium: PipeReader, limit: Duration) -> bool {
        let (tx, ended) = mpsc::channel();
        std::thread::spawn(move || {
            let mut rest = Vec::new();
            tx.send(from_chromium.read_to_end(&mut rest).map(|_| rest))
                .unwrap();
        });
        matches!(ended.recv_timeout(limit), Ok(Ok(rest)) if rest.is_empty())
    }

    #[test]
    fn attach_asks_for_the_pipe_and_spawn_closes_the_child_ends() {
        let mut cmd = exiting();
        let pipes = attach(&mut cmd).unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let pipe_flag = args.iter().position(|a| a == "--remote-debugging-pipe");
        let pipe_flag = pipe_flag.expect("--remote-debugging-pipe");
        if cfg!(windows) {
            let handles = args[pipe_flag + 1]
                .strip_prefix("--remote-debugging-io-pipes=")
                .unwrap();
            let (r, w) = handles.split_once(',').unwrap();
            assert!(
                r.parse::<u32>().is_ok() && w.parse::<u32>().is_ok(),
                "{handles}"
            );
        }
        let (mut child, from_chromium, _to_chromium) = pipes.spawn(&mut cmd).unwrap();
        child.wait().unwrap();
        // With the child gone and our copy of its write end closed, our
        // read end sees EOF.
        assert!(ends_within(from_chromium, Duration::from_secs(5)));
    }

    /// The bug this guards against: Chromium's ends were inheritable from
    /// `attach` on, and a child spawned before Chromium inherited them, so
    /// the pipe outlived the browser and that child could drive it.
    #[test]
    fn a_child_spawned_meanwhile_does_not_inherit_the_ends() {
        let mut cmd = exiting();
        let pipes = attach(&mut cmd).unwrap();
        let mut before = bystander();
        let (mut child, from_chromium, _to_chromium) = pipes.spawn(&mut cmd).unwrap();
        let mut after = bystander();
        child.wait().unwrap();
        let ended = ends_within(from_chromium, Duration::from_secs(5));
        for bystander in [&mut before, &mut after] {
            let _ = bystander.kill();
            let _ = bystander.wait();
        }
        assert!(ended, "the pipe ended with Chromium");
    }

    /// Chromium's spawn waits for every spawn in flight.
    #[test]
    fn spawn_waits_for_spawns_in_flight() {
        let mut cmd = exiting();
        let pipes = attach(&mut cmd).unwrap();
        let in_flight = spawn_gate::shared();
        let (tx, spawned) = mpsc::channel();
        let launch = std::thread::spawn(move || {
            let (mut child, _, _) = pipes.spawn(&mut cmd).unwrap();
            tx.send(()).unwrap();
            child.wait().unwrap();
        });
        assert_eq!(
            spawned.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "Chromium's spawn waits for the one in flight"
        );
        drop(in_flight);
        assert_eq!(spawned.recv_timeout(Duration::from_secs(5)), Ok(()));
        launch.join().unwrap();
    }

    /// The child really gets the pipes on the descriptors Chromium uses.
    #[cfg(unix)]
    #[test]
    fn a_unix_child_reads_fd_3_and_writes_fd_4() {
        use std::io::Write;
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "read line <&3; printf '%s!' \"$line\" >&4"]);
        let pipes = attach(&mut cmd).unwrap();
        let (mut child, mut from_chromium, mut to_chromium) = pipes.spawn(&mut cmd).unwrap();
        to_chromium.write_all(b"hello\n").unwrap();
        let mut got = String::new();
        from_chromium.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello!");
        assert!(child.wait().unwrap().success());
    }
}
