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
    pub fn drain(mut stderr: ChildStderr) -> Self {
        let kept = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&kept);
        let reader = std::thread::Builder::new()
            .name("browser-stderr".into())
            .spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n @ 1..) = stderr.read(&mut chunk) {
                    let mut kept = lock(&sink);
                    kept.extend_from_slice(&chunk[..n]);
                    // Trim in batches: amortised, and never above twice the cap.
                    if kept.len() > 2 * TAIL_BYTES {
                        let excess = kept.len() - TAIL_BYTES;
                        kept.drain(..excess);
                    }
                }
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

/// The last [`TAIL_BYTES`] of `bytes` as one line: lines trimmed, blank
/// ones dropped, joined with ` | `; a line cut at the front is dropped.
/// `None` when nothing is left.
fn tail_text(bytes: &[u8]) -> Option<String> {
    let cut = bytes.len().saturating_sub(TAIL_BYTES);
    let text = String::from_utf8_lossy(&bytes[cut..]);
    let text = if cut > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    join_nonblank(text.lines(), " | ")
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
