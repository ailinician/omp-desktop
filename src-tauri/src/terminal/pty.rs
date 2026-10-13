//! Spawning a shell on a pseudo-terminal, and the threads around it.
//!
//! **Tree kill.** `portable-pty` spawns through its own `CommandBuilder`, not
//! `std::process::Command`, so `ProcessSupervisor` does not fit: [`kill`]
//! kills by session id (Unix) instead, which also reaches background jobs the
//! shell left behind.
//!
//! **Threads per terminal.** A reader blocks on the master and forwards 8 KiB
//! chunks; a waiter blocks in `child.wait()` (Unix: `waitid` without reaping,
//! see [`kill`]); a writer feeds the master from
//! a bounded queue, so no command ever blocks on a full pty; a pump
//! ([`pump`]) coalesces what reader and waiter send, records it in the
//! terminal's output ring and emits it, under flow control. The reader
//! owns a duplicate of the master descriptor, so dropping the master does not
//! wake it: it ends at the pty's hangup (the shell, a session leader, exiting).
//! The pump still gives up waiting for it after a grace period, should a
//! platform keep the slave open through a background job.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtySize};

#[cfg(unix)]
use super::kill::wait_exit;
use super::kill::TreeKiller;
use super::pump::{pump, Emit, Msg, Stream, READER_GRACE};
use super::ring::Replay;
use crate::child_path;

/// How long closing a terminal waits for the hung-up shell to exit before it
/// kills the session (see [`Pty::close`]).
pub const HANGUP_GRACE: Duration = Duration::from_millis(200);
/// How often that wait checks for the exit.
const HANGUP_POLL: Duration = Duration::from_millis(5);
/// Shells that take `-l -i` (login, interactive).
const POSIX_SHELLS: [&str; 6] = ["sh", "bash", "zsh", "fish", "dash", "ksh"];
/// What the shell gets as `LANG` on macOS when the app has no locale (see
/// [`fallback_lang`]).
#[cfg(any(target_os = "macos", test))]
const FALLBACK_LANG: &str = "en_US.UTF-8";
/// One `read` from the master.
const READ_CHUNK: usize = 8 * 1024;
/// Chunks buffered between the reader and the pump (backpressure on a burst).
const QUEUE_DEPTH: usize = 64;
/// Writes queued for the shell; more than this and `write` fails.
const INPUT_DEPTH: usize = 1024;

/// What [`spawn`] starts: `program` with `shell_args` in `cwd`, on a
/// `cols`×`rows` pty, as shell `instance` of tab `session_id`.
pub struct SpawnSpec<'a> {
    pub session_id: &'a str,
    pub instance: u64,
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

    /// `None` while the shell runs; `Some(code)` once it exited (`code`
    /// `None` = killed by a signal).
    #[allow(clippy::option_option)] // the contract's shape, see `Stream::exit_code`
    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.stream.exit_code()
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

    /// End the terminal the way closing a terminal window does: silence it,
    /// hang the shell up, give it up to [`HANGUP_GRACE`] to exit, then
    /// [`Pty::kill`] what is left. Blocks up to the grace: call off the IPC
    /// thread. Several terminals share one grace through [`Pty::hang_up`],
    /// [`Pty::wait_exit_until`] and [`Pty::kill`].
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
        while self.stream.exit_code().is_none() && Instant::now() < deadline {
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

/// The shell to run and its arguments: the user's `preference` (which must be
/// an absolute path to an executable file; empty means unset), else
/// [`default_shell`]. Only the preference is an error — it was typed on
/// purpose.
pub fn resolve_shell(preference: Option<&str>) -> Result<(PathBuf, Vec<String>), String> {
    resolve_shell_with(preference, default_shell)
}

/// [`resolve_shell`] with the default passed in, so it needs no process env
/// or user database. `default` is only called for an unset preference.
pub fn resolve_shell_with(
    preference: Option<&str>,
    default: impl FnOnce() -> PathBuf,
) -> Result<(PathBuf, Vec<String>), String> {
    let preference = preference.map(str::trim).filter(|p| !p.is_empty());
    let shell = match preference {
        Some(path) => validate_shell(path)?,
        None => default(),
    };
    let posix = shell
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| POSIX_SHELLS.contains(&name));
    let args = if posix {
        vec!["-l".to_owned(), "-i".to_owned()]
    } else {
        Vec::new()
    };
    Ok((shell, args))
}

/// A shell setting the user typed: an absolute path to an executable file
/// (surrounding whitespace ignored). An empty setting means "default" and is
/// the caller's to handle; here it is refused like any other non-path.
pub fn validate_shell(path: &str) -> Result<PathBuf, String> {
    let path = path.trim();
    if is_executable_file(Path::new(path)) {
        Ok(PathBuf::from(path))
    } else {
        Err(format!(
            "terminal shell {path:?} is not an absolute path to an executable file"
        ))
    }
}

/// The shell an unset setting runs, chosen in-process by `portable-pty`:
/// on unix `$SHELL` when it is executable, else the account's login shell
/// from the user database (`getpwuid`), else `/bin/sh`; on Windows
/// `%ComSpec%`, else `cmd.exe`.
///
/// The user database matters because `$SHELL` is not guaranteed in the
/// app's environment (a launch from a script, some GUI launch paths): falling
/// straight to `/bin/sh` there starts a different shell than the user's, and
/// a `~/.profile` that launches the real shell stacks two shells, so `exit`
/// lands in the outer one.
pub fn default_shell() -> PathBuf {
    PathBuf::from(CommandBuilder::new_default_prog().get_shell())
}

fn is_executable_file(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_absolute()
            && path
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_absolute() && path.is_file()
    }
}

/// The `LANG` to give the shell when `is_set` says the app's environment
/// has none of `LANG`, `LC_ALL`, `LC_CTYPE`: [`FALLBACK_LANG`]. A macOS app
/// started from Finder or the Dock gets no locale (Terminal.app and iTerm2
/// set one themselves), and a shell without one runs in the C locale, where
/// non-ASCII input and output (`ls` of an accented file name, zsh's line
/// editor) breaks. A fixed UTF-8 locale is used rather than one derived from
/// the system's region (`defaults read -g AppleLocale`, a subprocess whose
/// value need not name an installed locale); a set variable is never
/// overridden.
#[cfg(any(target_os = "macos", test))]
fn fallback_lang(is_set: impl Fn(&str) -> bool) -> Option<&'static str> {
    let has_locale = ["LANG", "LC_ALL", "LC_CTYPE"].into_iter().any(is_set);
    (!has_locale).then_some(FALLBACK_LANG)
}

/// Start `spec.program` on a new pseudo-terminal and the threads that stream
/// its output through `emit`. `spec` is validated by the caller.
pub fn spawn<E: Emit>(emit: Arc<E>, spec: &SpawnSpec<'_>) -> Result<Pty, String> {
    let pair = native_pty_system()
        .openpty(size(spec.cols, spec.rows))
        .map_err(|e| format!("open pty: {e}"))?;

    let mut cmd = CommandBuilder::new(spec.program);
    cmd.args(spec.shell_args);
    cmd.cwd(spec.cwd);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");
    cmd.env("OMP_DESKTOP", "1");
    cmd.env("OMP_DESKTOP_SESSION", spec.session_id);
    if let Some(path) = child_path::child_path() {
        cmd.env("PATH", path);
    }
    #[cfg(target_os = "macos")]
    if let Some(lang) = fallback_lang(|key| cmd.get_env(key).is_some_and(|v| !v.is_empty())) {
        cmd.env("LANG", lang);
    }
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| format!("spawn {}: {e}", spec.program.display()))?;
    // Only the child may hold the slave, or the master never sees EOF.
    drop(pair.slave);

    #[cfg(unix)]
    let leader = child.process_id();
    #[cfg(unix)]
    let killer = Arc::new(TreeKiller::new(leader));
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
            let _ = child.wait(); // reap the zombie `kill` left
            return Err(format!("terminal streams: {e}"));
        }
    };

    let stream = Arc::new(Stream::new());
    let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
    let (input, input_rx) = mpsc::sync_channel(INPUT_DEPTH);
    let id = spec.session_id.to_owned(); // owned by the pump thread, which outlives `spec`
    let instance = spec.instance;

    let wait_tx = tx.clone(); // reader and waiter both feed the pump
    let wait_stream = Arc::clone(&stream);
    let wait_killer = Arc::clone(&killer);
    let started = spawn_thread("terminal-wait", move || {
        #[cfg(unix)]
        let code = wait_for_exit(&mut *child, leader, &wait_killer);
        #[cfg(windows)]
        let code = reaped_code(child.wait());
        // Before the pump hears of it: `Pty::exit_code` is current once the
        // exit event is out.
        wait_stream.set_exit(code);
        let _ = wait_tx.send(Msg::Exit(code));
        // End what the shell left behind and forget the session id now, not
        // at some later `close` or respawn. On unix the shell is still an
        // unreaped zombie here, so its pid, the session id, cannot belong to
        // anyone else. The grace period lets a job's last output reach the
        // reader first (see `pump::READER_GRACE`).
        thread::sleep(READER_GRACE);
        #[cfg(all(test, unix))]
        wait_killer.pause_point();
        wait_killer.kill();
        // Only now may the pid be reused: `kill` returns after every kill of
        // the session, also one a racing `close` was still running.
        #[cfg(unix)]
        let _ = child.wait();
    })
    .and_then(|()| spawn_thread("terminal-read", move || read_loop(reader, &tx)))
    .and_then(|()| spawn_thread("terminal-write", move || write_loop(writer, &input_rx)))
    .and_then(|()| {
        let pump_stream = Arc::clone(&stream);
        spawn_thread("terminal-emit", move || {
            pump(&rx, emit.as_ref(), &id, instance, &pump_stream);
        })
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

fn spawn_thread(name: &str, body: impl FnOnce() + Send + 'static) -> Result<(), String> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(body)
        .map(drop)
        .map_err(|e| format!("start {name} thread: {e}"))
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
fn wait_for_exit(
    child: &mut dyn portable_pty::Child,
    leader: Option<u32>,
    killer: &TreeKiller,
) -> Option<i32> {
    if let Some(Ok(code)) = leader.map(wait_exit) {
        return code;
    }
    killer.forget();
    reaped_code(child.wait())
}

/// Forward the master's output until EOF or an error, then tell the pump no
/// more is coming. Once the pump is gone (the terminal was closed, or it
/// stopped waiting for the reader) the output is still read, and dropped,
/// until EOF: on Windows before 11 24H2 `ClosePseudoConsole` (dropping the
/// master) blocks until the console's output pipe is drained, so a reader
/// that stopped early could hang the close for good.
fn read_loop(mut reader: Box<dyn Read + Send>, tx: &SyncSender<Msg>) {
    let mut buf = [0_u8; READ_CHUNK];
    let mut forwarding = true;
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if forwarding && tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                    forwarding = false;
                }
            }
        }
    }
    if forwarding {
        let _ = tx.send(Msg::ReaderDone);
    }
}

/// Write the queued input to the master, in order, until the terminal is
/// dropped or the write fails (the shell is gone).
fn write_loop(mut writer: Box<dyn Write + Send>, rx: &Receiver<Vec<u8>>) {
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

    /// The fallback (exit not observable without reaping) must forget the
    /// session id before reaping, so no later kill can signal a freed pid.
    #[test]
    fn fallback_reap_forgets_the_session_first() {
        let pty = native_pty_system().openpty(size(80, 24)).expect("openpty");
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

    /// `len` bytes of output, counting what was read.
    struct Output {
        left: usize,
        read: Arc<AtomicUsize>,
    }

    impl Read for Output {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.left.min(buf.len());
            self.left -= n;
            self.read.fetch_add(n, Ordering::Relaxed);
            Ok(n)
        }
    }

    /// With the pump gone, the reader still drains the pty to EOF (a
    /// pseudo console's close waits for that on older Windows).
    #[test]
    fn the_reader_drains_to_eof_after_the_pump_is_gone() {
        let read = Arc::new(AtomicUsize::new(0));
        let len = 10 * READ_CHUNK + 5;
        let (tx, rx) = mpsc::sync_channel(1);
        drop(rx);
        read_loop(
            Box::new(Output {
                left: len,
                read: Arc::clone(&read),
            }),
            &tx,
        );
        assert_eq!(read.load(Ordering::Relaxed), len);
    }

    /// While the pump listens, every chunk and then `ReaderDone` reach it.
    #[test]
    fn the_reader_forwards_every_chunk_then_reports_eof() {
        let len = 3 * READ_CHUNK;
        let (tx, rx) = mpsc::sync_channel(QUEUE_DEPTH);
        read_loop(
            Box::new(Output {
                left: len,
                read: Arc::new(AtomicUsize::new(0)),
            }),
            &tx,
        );
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
        assert_eq!((bytes, done), (len, true));
    }

    #[test]
    fn a_missing_locale_gets_the_utf8_fallback() {
        assert_eq!(fallback_lang(|_| false), Some("en_US.UTF-8"));
        for set in ["LANG", "LC_ALL", "LC_CTYPE"] {
            assert_eq!(fallback_lang(|key| key == set), None, "{set} is set");
        }
        assert_eq!(fallback_lang(|key| key == "LC_TIME"), Some("en_US.UTF-8"));
    }

    #[test]
    fn a_preference_wins_and_an_empty_one_runs_the_default() {
        let default = || PathBuf::from("/the/default");
        for empty in [None, Some(""), Some("  ")] {
            assert_eq!(
                resolve_shell_with(empty, default).unwrap().0,
                default(),
                "{empty:?}"
            );
        }
        let exe = std::env::current_exe().unwrap();
        let typed = format!(" {} ", exe.display());
        let called = std::cell::Cell::new(false);
        let (shell, args) = resolve_shell_with(Some(&typed), || {
            called.set(true);
            default()
        })
        .unwrap();
        assert_eq!(shell, exe, "trimmed preference");
        assert!(args.is_empty(), "not a POSIX shell: {args:?}");
        assert!(
            !called.get(),
            "the default is not resolved for a preference"
        );
        for bad in ["sh", "./sh", "/definitely/not/a/shell"] {
            assert!(resolve_shell_with(Some(bad), default).is_err(), "{bad}");
        }
    }

    /// The default comes from `portable-pty` in-process: an absolute path
    /// on unix (`$SHELL`, the user database or `/bin/sh`), `%ComSpec%` or
    /// `cmd.exe` on Windows.
    #[test]
    fn the_default_shell_is_resolved_in_process() {
        let shell = default_shell();
        #[cfg(unix)]
        assert!(is_executable_file(&shell), "{}", shell.display());
        #[cfg(windows)]
        assert!(
            shell
                .file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("cmd.exe")),
            "{}",
            shell.display()
        );
    }
}
