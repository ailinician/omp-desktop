//! The bodies of a terminal's reader, writer and waiter threads (the pump
//! is [`super::pump`]).

use std::io::{ErrorKind, Read, Write};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread;

use portable_pty::{Child, ExitStatus};

#[cfg(unix)]
use super::kill::wait_exit;
use super::kill::TreeKiller;
use super::pump::{Msg, READER_GRACE};
use super::stream::Stream;

/// One `read` from the master.
pub const READ_CHUNK: usize = 8 * 1024;

pub fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), String> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
        .map_err(|e| format!("start {name} thread: {e}"))
}

/// The waiter: block until the shell exits, record the exit and tell the
/// pump, then end what the shell left behind and forget the session id now,
/// not at some later `close` or respawn. On unix the shell is still an
/// unreaped zombie during that cleanup, so its pid, the session id, cannot
/// belong to anyone else; it is reaped last.
pub fn wait_loop(
    mut child: Box<dyn Child + Send + Sync>,
    killer: &TreeKiller,
    stream: &Stream,
    tx: &SyncSender<Msg>,
) {
    #[cfg(unix)]
    let code = {
        let leader = child.process_id();
        wait_for_exit(&mut *child, leader, killer)
    };
    #[cfg(windows)]
    let code = reaped_code(child.wait());
    // Before the pump hears of it: `Pty::state` is current once the exit
    // event is out.
    stream.set_exit(code);
    let _ = tx.send(Msg::Exit(code));
    // The grace period lets a job's last output reach the reader first (see
    // `pump::READER_GRACE`).
    thread::sleep(READER_GRACE);
    #[cfg(all(test, unix))]
    killer.pause_point();
    killer.kill();
    // Only now may the pid be reused: `kill` returns after every kill of the
    // session, also one a racing `close` was still running.
    #[cfg(unix)]
    let _ = child.wait();
}

/// The exit code to show for a reaped shell: none when it died by a signal
/// or the wait failed.
fn reaped_code(status: std::io::Result<ExitStatus>) -> Option<i32> {
    let status = status.ok()?;
    status
        .signal()
        .is_none()
        .then(|| i32::try_from(status.exit_code()).ok())
        .flatten()
}

/// Block until the shell has exited and return its exit code, leaving the
/// leader an unreaped zombie (see [`super::kill::wait_exit`]) so its pid
/// stays reserved while the session is cleaned up. If the exit cannot be
/// observed that way, the session id is forgotten *before* the shell is
/// reaped here: `forget` takes the same lock as `kill`, so it waits for an
/// in-flight kill and no kill can start afterwards — nothing ever signals a
/// pid that the reap has freed.
#[cfg(unix)]
fn wait_for_exit(child: &mut dyn Child, leader: Option<u32>, killer: &TreeKiller) -> Option<i32> {
    if let Some(Ok(code)) = leader.map(wait_exit) {
        return code;
    }
    killer.forget();
    reaped_code(child.wait())
}

/// Forward the master's output until EOF or an error, then tell the pump no
/// more is coming. A read interrupted by a signal is retried. Once the pump
/// is gone (the terminal was closed, or it stopped waiting for the reader)
/// the output is still read, and dropped, until EOF: on Windows before 11
/// 24H2 `ClosePseudoConsole` (dropping the master) blocks until the
/// console's output pipe is drained, so a reader that stopped early could
/// hang the close for good.
pub fn read_loop(mut reader: Box<dyn Read + Send>, tx: &SyncSender<Msg>) {
    let mut buf = [0_u8; READ_CHUNK];
    let mut forwarding = true;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if forwarding && tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                    forwarding = false;
                }
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    if forwarding {
        let _ = tx.send(Msg::ReaderDone);
    }
}

/// Write the queued input to the master, in order, until the terminal is
/// dropped or the write fails (the shell is gone).
pub fn write_loop(mut writer: Box<dyn Write + Send>, rx: &Receiver<Vec<u8>>) {
    while let Ok(data) = rx.recv() {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            return;
        }
    }
}

#[cfg(all(test, unix))]
mod wait_tests {
    use super::*;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};

    /// The fallback (exit not observable without reaping) must forget the
    /// session id before reaping, so no later kill can signal a freed pid.
    #[test]
    fn fallback_reap_forgets_the_session_first() {
        let pty = native_pty_system()
            .openpty(PtySize::default())
            .expect("openpty");
        let mut cmd = CommandBuilder::new("/bin/sh");
        cmd.args(["-c", "exit 7"]);
        let mut child = pty.slave.spawn_command(cmd).expect("spawn");
        let killer = TreeKiller::new(child.process_id());
        // `leader = None` forces the fallback path.
        let code = wait_for_exit(&mut *child, None, &killer);
        assert_eq!(code, Some(7));
        assert_eq!(killer.sid(), 0, "session id forgotten on the fallback path");
        killer.kill();
        assert_eq!(killer.signals(), 0, "no signal after the shell was reaped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc};

    /// `left` bytes of output, counting what was read; the first read
    /// fails with `EINTR` when `interrupt` is set.
    struct Output {
        left: usize,
        read: Arc<AtomicUsize>,
        interrupt: bool,
    }

    impl Output {
        fn new(left: usize) -> Self {
            Self {
                left,
                read: Arc::new(AtomicUsize::new(0)),
                interrupt: false,
            }
        }
    }

    impl Read for Output {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if std::mem::take(&mut self.interrupt) {
                return Err(ErrorKind::Interrupted.into());
            }
            let n = self.left.min(buf.len());
            self.left -= n;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    /// Everything `read_loop` sends for `output`: the data bytes, and
    /// whether `ReaderDone` came.
    fn forwarded(output: Output) -> (usize, bool) {
        let (tx, rx) = mpsc::sync_channel(64);
        read_loop(Box::new(output), &tx);
        drop(tx);
        let mut bytes = 0;
        let mut done = false;
        for msg in rx {
            match msg {
                Msg::Data(data) => bytes += data.len(),
                Msg::ReaderDone => done = true,
                Msg::Exit(_) => panic!("unexpected exit"),
            }
        }
        (bytes, done)
    }

    /// With the pump gone, the reader still drains the pty to EOF (a
    /// pseudo console's close waits for that on older Windows).
    #[test]
    fn the_reader_drains_to_eof_after_the_pump_is_gone() {
        let len = 10 * READ_CHUNK + 5;
        let output = Output::new(len);
        let read = Arc::clone(&output.read);
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        read_loop(Box::new(output), &tx);
        assert_eq!(read.load(Ordering::Relaxed), len);
    }

    /// While the pump listens, every chunk and then `ReaderDone` reach it.
    #[test]
    fn the_reader_forwards_every_chunk_then_reports_eof() {
        let len = 3 * READ_CHUNK;
        assert_eq!(forwarded(Output::new(len)), (len, true));
    }

    /// A read interrupted by a signal (`EINTR`) is retried, not taken for
    /// the end of the output.
    #[test]
    fn an_interrupted_read_is_retried() {
        let len = 2 * READ_CHUNK;
        let output = Output {
            interrupt: true,
            ..Output::new(len)
        };
        assert_eq!(forwarded(output), (len, true));
    }
}
