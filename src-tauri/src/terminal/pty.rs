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

use portable_pty::{native_pty_system, CommandBuilder, ExitStatus, MasterPty, PtySize};

#[cfg(unix)]
use super::kill::wait_exit;
use super::kill::TreeKiller;
use super::pump::{pump, Emit, Msg, Stream, READER_GRACE};
use super::ring::Replay;
use crate::child_path;

#[cfg(windows)]
pub const DEFAULT_SHELL: &str = "cmd.exe";
#[cfg(not(windows))]
pub const DEFAULT_SHELL: &str = "/bin/sh";

/// Shells that take `-l -i` (login, interactive).
const POSIX_SHELLS: [&str; 6] = ["sh", "bash", "zsh", "fish", "dash", "ksh"];
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

    /// Kill the shell and everything it started, and silence the terminal:
    /// no event follows, and a pump stalled on flow control exits at once.
    /// Blocks briefly (see [`TreeKiller`]).
    pub fn kill(&self) {
        self.stream.close();
        self.killer.kill();
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
/// an absolute path to an executable file; empty means unset), else `$SHELL`
/// when that is one, else the account's login shell from the user database,
/// else [`DEFAULT_SHELL`]. Only the preference is an error — it was typed on
/// purpose.
///
/// The account lookup matters because `$SHELL` is not guaranteed in the
/// app's environment (a launch from a script, some GUI launch paths): falling
/// straight to `/bin/sh` there starts a different shell than the user's, and
/// a `~/.profile` that launches the real shell stacks two shells, so `exit`
/// lands in the outer one.
pub fn resolve_shell(preference: Option<&str>) -> Result<(PathBuf, Vec<String>), String> {
    let env_shell = std::env::var("SHELL").ok();
    let usable_env = env_shell
        .as_deref()
        .is_some_and(|s| is_executable_file(Path::new(s)));
    let has_preference = preference.is_some_and(|p| !p.trim().is_empty());
    // Only looked up when it can decide: it spawns a short-lived process.
    let account = if usable_env || has_preference {
        None
    } else {
        account_shell()
    };
    resolve_shell_with(preference, env_shell.as_deref(), account.as_deref())
}

/// [`resolve_shell`] with `$SHELL` and the account shell passed in, so it
/// needs no process env or user database.
pub fn resolve_shell_with(
    preference: Option<&str>,
    env_shell: Option<&str>,
    account_shell: Option<&str>,
) -> Result<(PathBuf, Vec<String>), String> {
    let preference = preference.map(str::trim).filter(|p| !p.is_empty());
    let shell = if let Some(path) = preference {
        validate_shell(path)?
    } else {
        [env_shell, account_shell]
            .into_iter()
            .flatten()
            .map(Path::new)
            .find(|p| is_executable_file(p))
            .map_or_else(|| PathBuf::from(DEFAULT_SHELL), Path::to_path_buf)
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

/// The shell an unset setting runs: what `resolve_shell(None)` picks. May
/// look up the account's login shell (a short-lived process), so keep it
/// off the IPC thread.
pub fn default_shell() -> PathBuf {
    resolve_shell(None).map_or_else(|_| PathBuf::from(DEFAULT_SHELL), |(shell, _)| shell)
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

/// The current account's login shell from the user database: `dscl` on
/// macOS, `getent passwd` elsewhere on unix. `None` on any failure.
#[cfg(unix)]
pub(super) fn account_shell() -> Option<String> {
    let user = std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("/usr/bin/id")
                .arg("-un")
                .output()
                .ok()?;
            Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
        })?;
    let out = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/dscl")
            .args([".", "-read", &format!("/Users/{user}"), "UserShell"])
            .output()
            .ok()?
    } else {
        std::process::Command::new("getent")
            .args(["passwd", &user])
            .output()
            .ok()?
    };
    if !out.status.success() {
        return None;
    }
    parse_account_shell(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(unix))]
pub(super) const fn account_shell() -> Option<String> {
    None
}

/// `UserShell: /opt/homebrew/bin/fish` (dscl) or the 7th field of a passwd
/// line (getent).
pub fn parse_account_shell(output: &str) -> Option<String> {
    let line = output.lines().next()?.trim();
    let shell = line
        .strip_prefix("UserShell:")
        .map_or_else(|| line.rsplit(':').next().unwrap_or(""), str::trim);
    Some(shell.trim().to_owned()).filter(|s| s.starts_with('/'))
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

/// Forward the master's output until EOF, an error, or the pump going away;
/// then tell the pump no more is coming.
fn read_loop(mut reader: Box<dyn Read + Send>, tx: &SyncSender<Msg>) {
    let mut buf = [0_u8; READ_CHUNK];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.send(Msg::Data(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
        }
    }
    let _ = tx.send(Msg::ReaderDone);
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
