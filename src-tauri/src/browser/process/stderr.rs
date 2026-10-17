//! The end of a launched Chromium's stderr, kept to explain a failed start
//! ("No usable sandbox!", a missing library).

use std::io::Read as _;
use std::process::ChildStderr;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::browser::lock;
use crate::text::join_nonblank;

/// Bytes kept; everything before is drained and dropped.
const TAIL_BYTES: usize = 2048;

/// How long [`StderrTail::settle`] waits for the reader to see the end.
const SETTLE: Duration = Duration::from_millis(500);

/// The last [`TAIL_BYTES`] Chromium wrote to stderr. A thread drains the
/// pipe for the browser's whole life (a full pipe would block Chromium's
/// logging); only a failed start reads what it kept.
pub struct StderrTail {
    kept: Arc<Mutex<Vec<u8>>>,
    /// `None` when no thread could be started; the pipe then closes with
    /// the dropped closure and Chromium's writes to it fail harmlessly.
    reader: Option<JoinHandle<()>>,
}

impl StderrTail {
    /// Drain `stderr` on a thread; `on_end` runs there once the pipe ends
    /// (the browser's whole process tree closed it). Never runs when no
    /// thread could be started.
    pub fn drain(mut stderr: ChildStderr, on_end: impl FnOnce() + Send + 'static) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&kept);
        let reader = std::thread::Builder::new()
            .name("browser-stderr".into())
            .spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                    keep(&mut lock(&sink), &chunk[..n]);
                }
                on_end();
            });
        Self {
            kept,
            reader: reader.ok(),
        }
    }

    /// Once the process is gone: give the reader a moment to see the pipe
    /// close, then the kept lines as one line ([`tail_text`]).
    pub fn settle(self) -> Option<String> {
        let deadline = Instant::now() + SETTLE;
        while self.reader.as_ref().is_some_and(|r| !r.is_finished()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        tail_text(&lock(&self.kept))
    }
}

/// Append `chunk` to the tail. Trimmed in batches: amortised, and never
/// above twice the cap. One byte before the window stays, so [`tail_text`]
/// can tell whether its first line was cut.
fn keep(kept: &mut Vec<u8>, chunk: &[u8]) {
    kept.extend_from_slice(chunk);
    if kept.len() > 2 * TAIL_BYTES {
        let excess = kept.len() - (TAIL_BYTES + 1);
        kept.drain(..excess);
    }
}

/// The last [`TAIL_BYTES`] of `bytes` as one line: lines trimmed, blank
/// ones dropped, joined with ` | `; a line cut at the front is dropped,
/// unless it is all there is (one line longer than the window), which is
/// then kept clipped. `None` when nothing is left.
fn tail_text(bytes: &[u8]) -> Option<String> {
    let cut = bytes.len().saturating_sub(TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes[cut..]);
    let line_cut = cut > 0 && bytes[cut - 1] != b'\n';
    let whole = if line_cut {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    join_nonblank(whole.lines(), " | ").or_else(|| join_nonblank(text.lines(), " | "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    #[test]
    fn the_tail_keeps_the_last_whole_lines() {
        assert_eq!(tail_text(b""), None);
        assert_eq!(tail_text(b"\n  \n"), None);
        assert_eq!(
            tail_text(b"first\n\n  [1:2:ERROR] No usable sandbox!  \n"),
            Some("first | [1:2:ERROR] No usable sandbox!".into())
        );
        let mut long = "x".repeat(TAIL_BYTES).into_bytes();
        long.extend_from_slice(b"\nthe reason\n");
        assert_eq!(
            tail_text(&long),
            Some("the reason".into()),
            "cut line dropped"
        );
    }

    /// A trim that cuts a line and is followed by no more output: the cut
    /// fragment must not lead the reason.
    #[test]
    fn a_line_cut_by_the_reader_trim_is_dropped() {
        let mut kept = Vec::new();
        let mut chunk = "p".repeat(3 * TAIL_BYTES).into_bytes();
        chunk.extend_from_slice(b"\nthe reason\n");
        keep(&mut kept, &chunk);
        assert!(kept.len() <= 2 * TAIL_BYTES, "trimmed");
        assert_eq!(tail_text(&kept), Some("the reason".into()));
    }

    /// A window starting right after a newline holds a whole first line.
    #[test]
    fn a_whole_first_line_is_kept() {
        let mut bytes = b"earlier\n".to_vec();
        bytes.extend_from_slice("y".repeat(TAIL_BYTES - 1).as_bytes());
        bytes.push(b'\n');
        assert_eq!(tail_text(&bytes), Some("y".repeat(TAIL_BYTES - 1)));
    }

    /// A last line longer than the window is shown clipped, not lost.
    #[test]
    fn an_oversized_last_line_is_kept_clipped() {
        let mut bytes = b"earlier\n".to_vec();
        bytes.extend_from_slice("z".repeat(3 * TAIL_BYTES).as_bytes());
        assert_eq!(tail_text(&bytes), Some("z".repeat(TAIL_BYTES)));
    }

    /// A child writing `hi` to stderr, then waiting for its stdin to end.
    fn waiting_child() -> std::process::Child {
        #[cfg(unix)]
        let mut cmd = {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "echo hi >&2; cat"]);
            cmd
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut cmd = Command::new("cmd");
            cmd.args(["/C", "echo hi 1>&2 & findstr x"]);
            cmd
        };
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    /// `on_end` runs once stderr ends, not before; the tail is still kept.
    #[test]
    fn the_end_callback_runs_when_stderr_ends() {
        let mut child = waiting_child();
        let (tx, ended) = mpsc::channel();
        let tail = StderrTail::drain(child.stderr.take().unwrap(), move || {
            tx.send(()).unwrap();
        });
        assert_eq!(
            ended.recv_timeout(Duration::from_millis(300)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "stderr still open"
        );
        drop(child.stdin.take());
        child.wait().unwrap();
        assert_eq!(ended.recv_timeout(Duration::from_secs(5)), Ok(()));
        assert_eq!(tail.settle(), Some("hi".into()));
    }
}
