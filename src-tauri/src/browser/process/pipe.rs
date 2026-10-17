//! The pipe pair a launched Chromium speaks CDP over
//! (`--remote-debugging-pipe`): Chromium reads commands from one and writes
//! replies and events to the other, and opens no debugging port. Only this
//! process holds the far ends, so no other local process — of this user or
//! another — can reach the browser except through the broker.
//!
//! Unix: Chromium reads fd 3 and writes fd 4; the child's ends are moved
//! there between fork and exec. Windows: Chromium takes the two handle
//! values from `--remote-debugging-io-pipes=<read>,<write>`; the child's
//! ends are made inheritable for the spawn and closed here right after it.

use std::io::{self, PipeReader, PipeWriter};
use std::process::Command;
use std::thread::JoinHandle;

/// Our ends of a launched Chromium's CDP pipes, and the ends its process
/// takes over, until [`spawned`](Self::spawned).
pub struct CdpPipes {
    from_chromium: PipeReader,
    to_chromium: PipeWriter,
    /// Chromium's ends: what it reads commands from, what it writes to.
    child: (PipeReader, PipeWriter),
}

impl CdpPipes {
    /// The spawn is done (or failed): close our copies of the child's ends,
    /// so the pipes end when Chromium does, and hand over ours: what
    /// Chromium writes, what it reads.
    pub fn spawned(self) -> (PipeReader, PipeWriter) {
        let Self {
            from_chromium,
            to_chromium,
            child,
        } = self;
        drop(child);
        (from_chromium, to_chromium)
    }
}

/// Create the pipes and set `cmd` up to hand Chromium its ends.
pub fn attach(cmd: &mut Command) -> io::Result<CdpPipes> {
    let (child_reads, to_chromium) = io::pipe()?;
    let (from_chromium, child_writes) = io::pipe()?;
    cmd.arg("--remote-debugging-pipe");
    hand_over(cmd, &child_reads, &child_writes)?;
    Ok(CdpPipes {
        from_chromium,
        to_chromium,
        child: (child_reads, child_writes),
    })
}

/// Infallible here; the signature is the Windows one's.
#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn hand_over(cmd: &mut Command, reads: &PipeReader, writes: &PipeWriter) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    // SAFETY: the closure runs in the forked child before exec and calls
    // only async-signal-safe functions (`fcntl`, `dup2`, `close`) on fds
    // that are open in the child: `reads`/`writes` stay open in this process
    // until `CdpPipes::spawned`, which comes after the spawn returned. The
    // copies above 9 keep `dup2` from clobbering one end with the other when
    // either already is fd 3 or 4; `dup2` clears close-on-exec on 3 and 4,
    // while the originals (opened close-on-exec by `io::pipe`) and the
    // closed copies do not reach Chromium.
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
    Ok(())
}

#[cfg(windows)]
fn hand_over(cmd: &mut Command, reads: &PipeReader, writes: &PipeWriter) -> io::Result<()> {
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
    // An inherited handle keeps its value in the child. Handle values fit
    // in 32 bits (Chromium parses them as such).
    cmd.arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads.as_raw_handle() as usize,
        writes.as_raw_handle() as usize
    ));
    Ok(())
}

/// Wake `threads` when they are blocked reading or writing a CDP pipe, and
/// wait for them to end, so a broker that closed does not keep its pipe
/// threads. Windows only: there, another process spawned while Chromium's
/// ends were inheritable can hold a copy of them, so the pipe need not end
/// with Chromium. On Unix the ends are never inheritable, and the pipe ends
/// once Chromium is gone.
///
/// A cancel that lands just before a thread enters its blocking call is
/// lost, so it is repeated every 10 ms until every thread has finished, for
/// about a second at most. Never call it on one of `threads`.
#[cfg(windows)]
pub fn cancel_blocked_read(threads: &[&JoinHandle<()>]) {
    use std::time::{Duration, Instant};

    const RETRY: Duration = Duration::from_millis(10);
    const GIVE_UP: Duration = Duration::from_secs(1);

    let deadline = Instant::now() + GIVE_UP;
    loop {
        let mut running = false;
        for thread in threads.iter().filter(|thread| !thread.is_finished()) {
            running = true;
            cancel_synchronous_io(thread);
        }
        if !running || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(RETRY);
    }
}

#[cfg(windows)]
fn cancel_synchronous_io(thread: &JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::IO::CancelSynchronousIo;

    // SAFETY: the join handle keeps the thread handle open; cancelling I/O
    // of a thread that is not blocked (or already finished) is a no-op
    // that fails with ERROR_NOT_FOUND, which is ignored.
    unsafe {
        CancelSynchronousIo(thread.as_raw_handle() as HANDLE);
    }
}

#[cfg(unix)]
pub const fn cancel_blocked_read(_threads: &[&JoinHandle<()>]) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_asks_for_the_pipe_and_spawned_closes_the_child_ends() {
        let mut cmd = Command::new("chromium");
        let pipes = attach(&mut cmd).unwrap();
        let args: Vec<_> = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args[0], "--remote-debugging-pipe");
        if cfg!(windows) {
            let handles = args[1]
                .strip_prefix("--remote-debugging-io-pipes=")
                .unwrap();
            let (r, w) = handles.split_once(',').unwrap();
            assert!(
                r.parse::<u32>().is_ok() && w.parse::<u32>().is_ok(),
                "{handles}"
            );
        }
        let (mut from_chromium, _to_chromium) = pipes.spawned();
        // With the child's write end closed, our read end sees EOF.
        let mut buf = [0u8; 1];
        assert_eq!(
            std::io::Read::read(&mut from_chromium, &mut buf).unwrap(),
            0
        );
    }

    /// The child really gets the pipes on the descriptors Chromium uses.
    #[cfg(unix)]
    #[test]
    fn a_unix_child_reads_fd_3_and_writes_fd_4() {
        use std::io::{Read, Write};
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "read line <&3; printf '%s!' \"$line\" >&4"]);
        let pipes = attach(&mut cmd).unwrap();
        let mut child = cmd.spawn().unwrap();
        let (mut from_chromium, mut to_chromium) = pipes.spawned();
        to_chromium.write_all(b"hello\n").unwrap();
        let mut got = String::new();
        from_chromium.read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello!");
        assert!(child.wait().unwrap().success());
    }
}
