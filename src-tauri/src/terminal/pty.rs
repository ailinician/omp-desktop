//! Spawning a shell on a pseudo-terminal, and the [`Pty`] that owns it.
//!
//! **Tree kill.** `portable-pty` spawns through its own `CommandBuilder`, not
//! `std::process::Command`, so `ProcessSupervisor::prepare` does not fit:
//! [`super::kill`] kills by session id (Unix) instead, which also reaches
//! background jobs the shell left behind.
//!
//! **Threads per terminal** ([`super::threads`], [`super::pump`]). A reader
//! blocks on the master and forwards 8 KiB chunks; a waiter blocks until the
//! shell exits (Unix: `waitid` without reaping, see [`super::kill`]); a
//! writer feeds the master from a bounded queue, so no command ever blocks
//! on a full pty; a pump coalesces what reader and waiter send, records it
//! in the terminal's output ring and emits it, under flow control. The
//! reader owns a duplicate of the master descriptor, so dropping the master
//! does not wake it: it ends at EOF, which macOS delivers when the shell (a
//! session leader) exits and Linux only once no process holds the slave
//! open. The pump stops waiting for it after a grace period.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};

use super::kill::{self, TreeKiller};
use super::pump::{pump, TerminalEmit};
use super::ring::Replay;
use super::shell;
use super::stream::{ShellState, Stream};
use super::threads::{read_loop, spawn_thread, wait_loop, write_loop};

/// How long closing a terminal waits for the hung-up shell to exit before it
/// kills the session (see [`Pty::close`]).
pub const HANGUP_GRACE: Duration = Duration::from_millis(200);
/// How often that wait checks for the exit.
const HANGUP_POLL: Duration = Duration::from_millis(5);
/// Chunks buffered between the reader and the pump (backpressure on a burst).
const QUEUE_DEPTH: usize = 64;
/// Writes queued for the shell; more than this and `write` fails.
const INPUT_DEPTH: usize = 1024;

/// What [`spawn`] starts: `program` with `shell_args` in `cwd`, on a
/// `cols`×`rows` pty, for tab `session_id`.
pub struct SpawnSpec<'a> {
    pub session_id: &'a str,
    pub cwd: &'a Path,
    pub program: &'a Path,
    pub shell_args: &'a [String],
    pub cols: u16,
    pub rows: u16,
}

/// A running (or exited) shell: what `TerminalManager` keeps per tab, behind
/// an `Arc` so commands can use it without holding the manager's lock.
pub struct Pty {
    instance: u64,
    shell: PathBuf,
    killer: Arc<TreeKiller>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    input: SyncSender<Vec<u8>>,
    stream: Arc<Stream>,
}

impl Pty {
    /// For tests: the shell's tree killer.
    #[cfg(all(test, unix))]
    pub fn killer(&self) -> &TreeKiller {
        &self.killer
    }

    pub const fn instance(&self) -> u64 {
        self.instance
    }

    /// The program that was started.
    pub fn shell(&self) -> &Path {
        &self.shell
    }

    pub fn state(&self) -> ShellState {
        self.stream.shell_state()
    }

    /// Queue `data` for the shell. Never blocks: the writer thread does the
    /// actual (possibly blocking) write, in order.
    pub fn write(&self, data: Vec<u8>) -> Result<(), String> {
        self.input.try_send(data).map_err(|e| match e {
            TrySendError::Full(_) => "terminal input backlog".to_owned(),
            TrySendError::Disconnected(_) => "terminal input is closed".to_owned(),
        })
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<(), String> {
        self.master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .resize(size(cols, rows))
            .map_err(|e| format!("resize terminal: {e}"))
    }

    /// Kill the shell and everything it started at once (SIGKILL), and
    /// silence the terminal: no event follows, and a pump stalled on flow
    /// control exits at once. Blocks briefly (see [`TreeKiller`]).
    pub fn kill(&self) {
        self.stream.close();
        self.killer.kill();
    }

    /// [`Pty::kill`] for several terminals, with one process-table scan per
    /// pass for all of them (see [`kill::kill_all`]).
    pub fn kill_all(ptys: &[Arc<Self>]) {
        for pty in ptys {
            pty.stream.close();
        }
        let killers: Vec<&TreeKiller> = ptys.iter().map(|pty| &*pty.killer).collect();
        kill::kill_all(&killers);
    }

    /// End the terminal the way closing a terminal window does: silence it,
    /// hang the shell up, give it up to [`HANGUP_GRACE`] to exit, then
    /// [`Pty::kill`] what is left. Blocks up to the grace: call off the IPC
    /// thread. Several terminals share one grace through [`Pty::hang_up`],
    /// [`Pty::wait_exit_until`] and [`Pty::kill_all`].
    pub fn close(&self) {
        if self.hang_up() {
            self.wait_exit_until(Instant::now() + HANGUP_GRACE);
        }
        self.kill();
    }

    /// Silence the terminal and hang the shell up (unix: SIGHUP to its
    /// process group; Windows has no hang-up, the job kill ends it). Returns
    /// whether there is an exit worth waiting for.
    pub fn hang_up(&self) -> bool {
        self.stream.close();
        #[cfg(unix)]
        {
            self.killer.hang_up()
        }
        #[cfg(windows)]
        {
            false
        }
    }

    /// Wait until the shell has exited or `deadline` has passed.
    pub fn wait_exit_until(&self, deadline: Instant) {
        while self.state() == ShellState::Running && Instant::now() < deadline {
            thread::sleep(HANGUP_POLL);
        }
    }

    /// The held output after event `after_seq` (see [`super::ring`]). Also
    /// forgives the flow-control debt: the caller renders this instead of
    /// acking the events it covers.
    pub fn replay(&self, after_seq: u64) -> Replay {
        self.stream.replay(after_seq)
    }

    /// The webview has rendered `bytes` more of the emitted output.
    pub fn ack(&self, bytes: u64) {
        self.stream.ack(bytes);
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Safety net for an owner that never called `kill` (a no-op after it).
        self.kill();
    }
}

const fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Start `spec.program` on a new pseudo-terminal as shell `instance`, and
/// the threads that stream its output through `emit`. `spec` is validated
/// by the caller.
pub fn spawn(
    emit: impl TerminalEmit + Send + 'static,
    instance: u64,
    spec: &SpawnSpec<'_>,
) -> Result<Pty, String> {
    let pair = native_pty_system()
        .openpty(size(spec.cols, spec.rows))
        .map_err(|e| format!("open pty: {e}"))?;

    let mut cmd = CommandBuilder::new(spec.program);
    cmd.args(spec.shell_args);
    cmd.cwd(spec.cwd);
    shell::apply_env(&mut cmd, spec.program, spec.session_id);
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {}: {e}", spec.program.display()))?;
    // Only the child may hold the slave, or the master never sees EOF.
    drop(pair.slave);

    #[cfg(unix)]
    let killer = Arc::new(TreeKiller::new(child.process_id()));
    #[cfg(windows)]
    let killer = Arc::new(TreeKiller::new(child.clone_killer(), child.as_raw_handle()));

    // No `Pty` owns the shell yet: a failure from here on must kill it
    // explicitly, or it would outlive the failed open.
    let streams = pair
        .master
        .try_clone_reader()
        .and_then(|reader| Ok((reader, pair.master.take_writer()?)));
    let (reader, writer) = match streams {
        Ok(streams) => streams,
        Err(e) => {
            killer.kill();
            #[cfg(unix)]
            {
                // Reap the zombie `kill` left.
                let mut child = child;
                let _ = child.wait();
            }
            return Err(format!("terminal streams: {e}"));
        }
    };

    let stream = Arc::new(Stream::new());
    let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
    let (input, input_rx) = mpsc::sync_channel(INPUT_DEPTH);

    let wait_tx = tx.clone(); // reader and waiter both feed the pump
    let wait_stream = Arc::clone(&stream);
    let wait_killer = Arc::clone(&killer);
    let started = spawn_thread("terminal-wait", move || {
        wait_loop(child, &wait_killer, &wait_stream, &wait_tx);
    })
    .and_then(|()| spawn_thread("terminal-read", move || read_loop(reader, &tx)))
    .and_then(|()| spawn_thread("terminal-write", move || write_loop(writer, &input_rx)))
    .and_then(|()| {
        let pump_stream = Arc::clone(&stream);
        spawn_thread("terminal-emit", move || pump(&rx, emit, &pump_stream))
    });
    if let Err(e) = started {
        stream.close();
        killer.kill();
        return Err(e);
    }

    Ok(Pty {
        instance,
        shell: spec.program.to_path_buf(),
        killer,
        master: Mutex::new(pair.master),
        input,
        stream,
    })
}
