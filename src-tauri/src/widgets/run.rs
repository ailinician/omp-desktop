//! Run one widget plugin: argv from the validated config, context via env +
//! stdin JSON, stdout capped, wall-clock timeout, process tree killed on either.
//! Synchronous and blocking — callers run it on `spawn_blocking`.
//!
//! A blocking `read` on the child's stdout cannot observe the deadline while
//! the plugin is idle, so the pipes are drained on dedicated threads and the
//! calling thread polls `try_wait` against the deadline every [`POLL`]. The
//! stdout reader stops and raises a shared flag once [`STDOUT_CAP_BYTES`] is
//! passed; the poll loop sees the flag and kills the child. Reader results
//! come back over channels so a grandchild that inherited a pipe and outlives
//! the plugin can only delay the result until the deadline, never past it.
//!
//! The context is written to stdin on its own thread, started after the
//! readers, so the caller never waits on the write; the poll loop alone owns
//! the deadline. That thread is never joined.
//!
//! Every [`PluginContext`] string except `project` is bounded to
//! [`CONTEXT_FIELD_MAX_BYTES`] in [`context`]. Each value is also exported as an
//! environment variable, and Linux limits one env string to 128 KiB, so an
//! unbounded value would fail `spawn` with E2BIG. `project` is left intact: it
//! is the plugin's working directory, so truncating it would change where the
//! plugin runs, and the OS path limit already bounds it. The stdin payload is
//! therefore at most `PATH_MAX` + 4 x 2048 bytes plus keys, far below any exec
//! limit. On timeout or cap the whole process tree is killed (see
//! [`ProcessSupervisor`]), which closes the plugin's stdin read end, so the
//! detached stdin writer always fails and returns.
//! stderr keeps a rolling tail of [`STDERR_KEEP_BYTES`] (the end is what
//! explains a failure) while the rest is drained and dropped.
//!
//! Every thread is created with `thread::Builder`. If the OS refuses one, the
//! child is killed and reaped and the run fails with
//! `could not start reader thread: …` instead of panicking out of the caller.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::agent::supervisor::ProcessSupervisor;
use crate::widgets::config::PluginEntry;

pub const STDOUT_CAP_BYTES: usize = 256 * 1024;
/// How much stderr is kept for error messages; the rest is drained and dropped.
const STDERR_KEEP_BYTES: usize = 64 * 1024;
/// How much of a failing run's stderr (or stdout) the error keeps.
const ERR_TAIL_CHARS: usize = 300;
const POLL: Duration = Duration::from_millis(25);
/// Upper bound for every [`PluginContext`] string, see the module docs.
const CONTEXT_FIELD_MAX_BYTES: usize = 2048;

#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginContext {
    pub project: String,
    pub branch: String,
    pub remote: String,
    pub session: String,
    pub trigger: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunResult {
    pub plugin_id: String,
    pub stdout: Option<String>,
    pub error: Option<String>,
    pub duration_ms: u64,
}

/// Branch and default-remote fetch URL for `project` (empty when absent).
pub fn context(project: &Path, session: &str, trigger: &str) -> PluginContext {
    let (branch, _) = crate::git::probe(project);
    let remote = gix::discover(project).ok().and_then(|repo| {
        let url = repo
            .find_default_remote(gix::remote::Direction::Fetch)
            .and_then(Result::ok)
            .and_then(|r| {
                r.url(gix::remote::Direction::Fetch)
                    .map(|u| u.to_bstring().to_string())
            });
        url
    });
    bounded_context(
        project.to_string_lossy().into_owned(),
        branch.unwrap_or_default(),
        remote.unwrap_or_default(),
        session.to_string(),
        trigger.to_string(),
    )
}

/// Assemble the context, bounding every string except `project` to
/// [`CONTEXT_FIELD_MAX_BYTES`]. `project` is the plugin's cwd: truncating it
/// would change where the plugin runs, and the OS path limit already bounds it.
fn bounded_context(
    project: String,
    branch: String,
    remote: String,
    session: String,
    trigger: String,
) -> PluginContext {
    let mut ctx = PluginContext {
        project,
        branch,
        remote,
        session,
        trigger,
    };
    for field in [
        &mut ctx.branch,
        &mut ctx.remote,
        &mut ctx.session,
        &mut ctx.trigger,
    ] {
        truncate_field(field);
    }
    ctx
}

/// Cut `field` to at most [`CONTEXT_FIELD_MAX_BYTES`] on a char boundary.
fn truncate_field(field: &mut String) {
    if field.len() <= CONTEXT_FIELD_MAX_BYTES {
        return;
    }
    let mut end = CONTEXT_FIELD_MAX_BYTES;
    while !field.is_char_boundary(end) {
        end -= 1;
    }
    field.truncate(end);
}

pub fn run_plugin(entry: &PluginEntry, ctx: &PluginContext) -> RunResult {
    let start = Instant::now();
    let (stdout, error) = match run_inner(entry, ctx) {
        Ok(s) => (Some(s), None),
        Err(e) => (None, Some(e)),
    };
    RunResult {
        plugin_id: entry.id.clone(), // the result is keyed by id on the JS side; the entry outlives the call
        stdout,
        error,
        duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}

fn run_inner(entry: &PluginEntry, ctx: &PluginContext) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_millis(entry.timeout_ms);
    let (mut child, supervisor) = spawn(entry, ctx)?;

    let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        terminate(&supervisor, &mut child);
        return Err("stdout or stderr pipe missing".to_string());
    };
    let overflow = Arc::new(AtomicBool::new(false));
    let stdout_rx = spawn_drain(
        out_pipe,
        STDOUT_CAP_BYTES,
        Some(Arc::clone(&overflow)), // the reader thread raises the flag, this thread polls it
    )
    .inspect_err(|_| terminate(&supervisor, &mut child))?;
    let stderr_rx = spawn_drain(err_pipe, STDERR_KEEP_BYTES, None)
        .inspect_err(|_| terminate(&supervisor, &mut child))?;
    feed_stdin(&mut child, ctx).inspect_err(|_| terminate(&supervisor, &mut child))?;

    let status = wait_for_exit(
        &mut child,
        &supervisor,
        &overflow,
        deadline,
        entry.timeout_ms,
    )?;
    let out = recv_until(&stdout_rx, deadline)
        .ok_or_else(|| format!("timed out after {} ms", entry.timeout_ms))?
        .map_err(|e| format!("could not read stdout: {e}"))?;
    if overflow.load(Ordering::Relaxed) {
        return Err(cap_error());
    }
    if !status.success() {
        let stderr = recv_until(&stderr_rx, deadline)
            .and_then(Result::ok)
            .unwrap_or_default();
        let tail_src = if stderr.is_empty() { &out } else { &stderr };
        return Err(format!("exit {}: {}", status, tail(tail_src)));
    }
    String::from_utf8(out).map_err(|_| "stdout is not UTF-8".to_string())
}

fn spawn(entry: &PluginEntry, ctx: &PluginContext) -> Result<(Child, ProcessSupervisor), String> {
    let Some((argv0, rest)) = entry.command.split_first() else {
        return Err("command is empty".to_string());
    };
    let cwd = if ctx.project.is_empty() {
        std::env::temp_dir()
    } else {
        std::path::PathBuf::from(&ctx.project)
    };
    let mut cmd = Command::new(argv0);
    cmd.args(rest)
        .current_dir(cwd)
        .env("OMP_WIDGET_VERSION", "1")
        .env("OMP_WIDGET_PROJECT", &ctx.project)
        .env("OMP_WIDGET_BRANCH", &ctx.branch)
        .env("OMP_WIDGET_REMOTE", &ctx.remote)
        .env("OMP_WIDGET_SESSION", &ctx.session)
        .env("OMP_WIDGET_TRIGGER", &ctx.trigger)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::child_path::apply_child_path(&mut cmd);
    ProcessSupervisor::prepare(&mut cmd);
    let child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("command not found: {argv0}"),
        _ => format!("could not start {argv0}: {e}"),
    })?;
    let supervisor = ProcessSupervisor::attach(&child);
    Ok((child, supervisor))
}

/// Context on stdin from a detached thread, which closes the pipe when done so
/// `cat`-style plugins terminate. Never joined. If the plugin never reads, the
/// write may block, but only until the run ends: timeout and cap kill the whole
/// process tree, every holder of the pipe's read end dies, and `write_all`
/// fails and returns.
fn feed_stdin(child: &mut Child, ctx: &PluginContext) -> Result<(), String> {
    let Some(mut stdin) = child.stdin.take() else {
        return Ok(());
    };
    let payload = serde_json::json!({
        "version": 1, "project": ctx.project, "branch": ctx.branch,
        "remote": ctx.remote, "session": ctx.session, "trigger": ctx.trigger,
    })
    .to_string();
    std::thread::Builder::new()
        .name("widget-stdin".to_string())
        .spawn(move || {
            // Invariant: the run never leaves a descendant holding the read end
            // after it ends (`kill_tree`), so this write cannot block forever.
            let _ = stdin.write_all(payload.as_bytes()); // a plugin that closed stdin early gives EPIPE; that's fine
        }) // `stdin` drops at the end of the closure, closing the pipe
        .map(drop) // detached on purpose; the handle is not needed
        .map_err(|e| format!("could not start stdin thread: {e}"))
}

/// Drain `pipe` on a thread. With `overflow`, at most `keep` bytes are kept
/// from the start; exceeding that raises the flag and stops reading (the
/// caller kills the child). Without it the last `keep` bytes are kept and the
/// rest is read and discarded so the writer never blocks on a full pipe.
fn spawn_drain(
    mut pipe: impl Read + Send + 'static,
    keep: usize,
    overflow: Option<Arc<AtomicBool>>,
) -> Result<Receiver<std::io::Result<Vec<u8>>>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("widget-drain".to_string())
        .spawn(move || {
            let mut buf: VecDeque<u8> = VecDeque::new();
            let mut chunk = [0u8; 8192];
            let result = loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break Ok(Vec::from(buf)),
                    Ok(n) => {
                        if let Some(flag) = &overflow {
                            if buf.len() + n > keep {
                                flag.store(true, Ordering::Relaxed);
                                break Ok(Vec::from(buf));
                            }
                            buf.extend(&chunk[..n]);
                        } else {
                            buf.extend(&chunk[..n]);
                            let excess = buf.len().saturating_sub(keep);
                            buf.drain(..excess);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => break Err(e),
                }
            };
            let _ = tx.send(result); // the receiver is gone after a timeout; nobody wants the bytes then
        })
        .map_err(|e| format!("could not start reader thread: {e}"))?;
    Ok(rx)
}

/// Poll until the child exits. Kills the process tree and errors on deadline
/// or when the stdout reader reports the cap was passed.
fn wait_for_exit(
    child: &mut Child,
    supervisor: &ProcessSupervisor,
    overflow: &AtomicBool,
    deadline: Instant,
    timeout_ms: u64,
) -> Result<ExitStatus, String> {
    loop {
        if overflow.load(Ordering::Relaxed) {
            terminate(supervisor, child);
            return Err(cap_error());
        }
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                terminate(supervisor, child);
                return Err(format!("timed out after {timeout_ms} ms"));
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(e) => {
                terminate(supervisor, child);
                return Err(format!("wait failed: {e}"));
            }
        }
    }
}

/// Result of a drain thread, or `None` if it has not finished by `deadline`
/// (or died without sending).
fn recv_until(
    rx: &Receiver<std::io::Result<Vec<u8>>>,
    deadline: Instant,
) -> Option<std::io::Result<Vec<u8>>> {
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .ok()
}

/// Kill the plugin's process tree, then reap the direct child.
///
/// `ProcessSupervisor::kill_tree` is a no-op when the supervisor could not be
/// attached (on Windows, creating or assigning the Job Object can fail). The
/// direct-child `kill` is the fallback so the following `wait` cannot block on a
/// plugin that is still running. After a successful tree kill it is idempotent:
/// on an already-dead process `Child::kill` returns `Ok` or `InvalidInput`, and
/// both are ignored.
fn terminate(supervisor: &ProcessSupervisor, child: &mut Child) {
    supervisor.kill_tree();
    let _ = child.kill();
    let _ = child.wait(); // reap; nothing useful to do on failure
}

fn cap_error() -> String {
    format!("stdout exceeds {} KiB", STDOUT_CAP_BYTES / 1024)
}

fn tail(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let s = s.trim();
    let count = s.chars().count();
    if count <= ERR_TAIL_CHARS {
        s.to_string()
    } else {
        s.chars().skip(count - ERR_TAIL_CHARS).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::widgets::config::PluginEntry;

    fn entry(cmd: &[&str], timeout_ms: u64) -> PluginEntry {
        PluginEntry {
            id: "t".into(),
            title: "T".into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            match_path: None,
            match_remote: None,
            triggers: vec!["manual".into()],
            timeout_ms,
            enabled: true,
        }
    }

    fn ctx() -> PluginContext {
        PluginContext {
            project: std::env::temp_dir().to_string_lossy().into_owned(),
            branch: String::new(),
            remote: String::new(),
            session: "s1".into(),
            trigger: "manual".into(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn happy_path_returns_stdout_and_env() {
        let r = run_plugin(
            &entry(
                &[
                    "sh",
                    "-c",
                    "echo \"{\\\"trigger\\\":\\\"$OMP_WIDGET_TRIGGER\\\"}\"",
                ],
                5_000,
            ),
            &ctx(),
        );
        assert_eq!(r.error, None, "{r:?}");
        assert_eq!(
            r.stdout.as_deref().map(str::trim),
            Some(r#"{"trigger":"manual"}"#)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdin_carries_context_json() {
        let r = run_plugin(&entry(&["sh", "-c", "cat"], 5_000), &ctx());
        let v: serde_json::Value =
            serde_json::from_str(r.stdout.as_deref().unwrap_or("")).expect("json on stdin");
        assert_eq!(v["session"], "s1");
        assert_eq!(v["version"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn nonzero_exit_is_error_even_with_json() {
        let r = run_plugin(
            &entry(
                &["sh", "-c", "echo '{\"version\":1}'; echo boom >&2; exit 3"],
                5_000,
            ),
            &ctx(),
        );
        assert!(r.stdout.is_none());
        let e = r.error.expect("error");
        assert!(e.contains("exit") && e.contains("boom"), "{e}");
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_and_errors() {
        let start = std::time::Instant::now();
        let r = run_plugin(&entry(&["sh", "-c", "sleep 30"], 1_000), &ctx());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "must not wait for sleep"
        );
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 1000 ms")),
            "{r:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stdout_cap_kills_and_errors() {
        let r = run_plugin(
            &entry(
                &["sh", "-c", "yes 0123456789abcdef | head -c 1048576"],
                5_000,
            ),
            &ctx(),
        );
        assert!(
            r.error.as_deref().is_some_and(|e| e.contains("exceeds")),
            "{r:?}"
        );
        assert!(r.stdout.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn max_size_context_does_not_block_a_plugin_that_ignores_stdin() {
        let mut full = ctx();
        full.session = "x".repeat(CONTEXT_FIELD_MAX_BYTES);
        full.remote = "x".repeat(CONTEXT_FIELD_MAX_BYTES);
        full.branch = "x".repeat(CONTEXT_FIELD_MAX_BYTES);
        let start = std::time::Instant::now();
        let r = run_plugin(&entry(&["sh", "-c", "sleep 30"], 1_000), &full);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "must not block on stdin"
        );
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 1000 ms")),
            "{r:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn timeout_kills_grandchild_and_closes_stdin() {
        let pidfile = std::env::temp_dir().join(format!("omp-widget-gc-{}", std::process::id()));
        let script = format!("sleep 30 & echo $! > '{}'; wait", pidfile.display());
        let r = run_plugin(&entry(&["sh", "-c", &script], 1_000), &ctx());
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("timed out after 1000 ms")),
            "{r:?}"
        );
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile)
            .expect("plugin wrote the sleep pid")
            .trim()
            .parse()
            .expect("pid");
        let _ = std::fs::remove_file(&pidfile);
        // `kill(pid, 0)` probes existence without signalling; a killed
        // grandchild is reaped by init a moment later, so poll briefly.
        let deadline = Instant::now() + Duration::from_secs(3);
        let alive = loop {
            // SAFETY: plain-integer FFI call; signal 0 only probes existence.
            let alive = unsafe { libc::kill(pid, 0) == 0 };
            if !alive || Instant::now() >= deadline {
                break alive;
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(!alive, "sleep grandchild {pid} survived the timeout");
    }

    #[test]
    fn context_fields_are_bounded() {
        let mut s = "x".repeat(100 * 1024);
        truncate_field(&mut s);
        assert_eq!(s.len(), CONTEXT_FIELD_MAX_BYTES);

        // 2-byte chars, 3000 bytes: the cut must keep only whole chars.
        let mut umlauts = "ä".repeat(1500);
        truncate_field(&mut umlauts);
        assert!(umlauts.len() <= CONTEXT_FIELD_MAX_BYTES);
        let bytes = umlauts.into_bytes();
        let text = String::from_utf8(bytes).expect("cut left valid UTF-8");
        assert_eq!(text.chars().last(), Some('ä'), "last char is complete");

        // 3-byte chars: the cap falls inside a char, so the cut must back off.
        let mut multi = "€".repeat(CONTEXT_FIELD_MAX_BYTES);
        truncate_field(&mut multi);
        assert!(multi.len() <= CONTEXT_FIELD_MAX_BYTES);
        assert!(multi.chars().all(|c| c == '€'));

        let mut short = "abc".to_string();
        truncate_field(&mut short);
        assert_eq!(short, "abc");
    }

    #[test]
    fn context_bounds_all_but_project() {
        let long = || "x".repeat(100 * 1024);
        // Longer than the field bound: the path is carried through unchanged.
        let c = bounded_context(
            "p".repeat(3 * CONTEXT_FIELD_MAX_BYTES),
            long(),
            long(),
            long(),
            long(),
        );
        assert_eq!(
            c.project.len(),
            3 * CONTEXT_FIELD_MAX_BYTES,
            "project is untouched"
        );
        assert!(c.project.bytes().all(|b| b == b'p'));
        for (name, field) in [
            ("branch", &c.branch),
            ("remote", &c.remote),
            ("session", &c.session),
            ("trigger", &c.trigger),
        ] {
            assert_eq!(field.len(), CONTEXT_FIELD_MAX_BYTES, "{name} bounded");
        }
    }

    #[cfg(unix)]
    #[test]
    fn stderr_keeps_the_tail_not_the_head() {
        let r = run_plugin(
            &entry(
                &[
                    "sh",
                    "-c",
                    "yes log | head -c 200000 >&2; echo FINAL-MARKER >&2; exit 2",
                ],
                5_000,
            ),
            &ctx(),
        );
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("FINAL-MARKER")),
            "{r:?}"
        );
    }

    #[test]
    fn missing_executable_is_not_found_error() {
        let r = run_plugin(&entry(&["omp-widget-does-not-exist-9f3a"], 5_000), &ctx());
        assert!(
            r.error
                .as_deref()
                .is_some_and(|e| e.contains("command not found")),
            "{r:?}"
        );
    }

    #[test]
    fn context_outside_repo_has_empty_branch_and_remote() {
        let c = context(&std::env::temp_dir(), "s", "activate");
        assert_eq!(c.branch, "");
        assert_eq!(c.remote, "");
        assert_eq!(c.trigger, "activate");
    }

    #[test]
    fn context_in_repo_has_branch_and_fetch_url() {
        let dir = std::env::temp_dir().join(format!("omp-widgets-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("tmp dir");
        gix::init(&dir).expect("git init");
        let config = dir.join(".git/config");
        let mut text = std::fs::read_to_string(&config).expect("read config");
        text.push_str("[remote \"origin\"]\n\turl = https://example.test/o/r.git\n");
        std::fs::write(&config, text).expect("write config");
        let c = context(&dir, "s", "activate");
        assert_eq!(c.remote, "https://example.test/o/r.git");
        assert!(!c.branch.is_empty(), "{c:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
