//! The pump: turns what the reader and waiter threads send into events.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use super::Emit;

/// Largest payload of one event.
pub const MAX_EVENT: usize = 64 * 1024;
/// Minimum spacing between two events of one terminal.
pub const MIN_INTERVAL: Duration = Duration::from_millis(4);
/// How long after the shell exited the reader may take to reach EOF. On Unix
/// the pty hangs up when its session leader exits, so EOF follows at once; a
/// platform where a background job holding the slave open could keep the
/// reader from ever getting there is covered by abandoning the reader after
/// this, dropping its late output.
pub const READER_GRACE: Duration = Duration::from_millis(250);

/// What the terminal's threads tell the pump.
pub enum Msg {
    /// Output from the master.
    Data(Vec<u8>),
    /// The reader reached EOF or failed: no more data follows.
    ReaderDone,
    /// The shell exited and was reaped; `None` = killed by a signal.
    Exit(Option<i32>),
}

/// Liveness of one terminal, shared with its threads.
pub struct Flags {
    /// The shell has not exited yet.
    pub alive: AtomicBool,
    /// Owner gone (closed, replaced, shut down): emit nothing any more.
    pub closed: AtomicBool,
}

impl Flags {
    pub const fn new() -> Self {
        Self {
            alive: AtomicBool::new(true),
            closed: AtomicBool::new(false),
        }
    }
}

/// Emit the output as events: one per [`MIN_INTERVAL`] at the most (the
/// first after a quiet spell goes out at once), each of at most
/// [`MAX_EVENT`] bytes — a larger backlog is emitted piecewise, one piece
/// per interval, and holds the reader back meanwhile. Then the exit event,
/// once the shell is reaped, the reader is done (or abandoned after
/// [`READER_GRACE`]) and everything is out. A closed terminal emits nothing.
pub fn pump<E: Emit>(rx: &Receiver<Msg>, emit: &E, id: &str, flags: &Flags) {
    let mut pending: Vec<u8> = Vec::new();
    let mut last_emit: Option<Instant> = None;
    let mut reader_done = false;
    let mut shell_exit: Option<(Instant, Option<i32>)> = None;
    loop {
        if flags.closed.load(Ordering::Acquire) {
            return;
        }
        let now = Instant::now();
        if let Some((at, _)) = shell_exit {
            if !reader_done && now >= at + READER_GRACE {
                // Abandoned: keep what already arrived, nothing more.
                while let Ok(msg) = rx.try_recv() {
                    if let Msg::Data(chunk) = msg {
                        pending.extend_from_slice(&chunk);
                    }
                }
                reader_done = true;
            }
            if reader_done && pending.is_empty() {
                break;
            }
        }
        let emit_at = (!pending.is_empty()).then(|| last_emit.map_or(now, |t| t + MIN_INTERVAL));
        if emit_at.is_some_and(|t| t <= now) {
            let len = pending.len().min(MAX_EVENT);
            emit.data(id, &pending[..len]);
            pending.drain(..len);
            last_emit = Some(Instant::now());
            continue;
        }
        let grace_at = shell_exit
            .filter(|_| !reader_done)
            .map(|(at, _)| at + READER_GRACE);
        let wake = [emit_at, grace_at].into_iter().flatten().min();
        // A full event is waiting for its slot: leave the rest in the
        // channel (the reader blocks on it). Likewise once all is received.
        let receiving = pending.len() < MAX_EVENT && !(shell_exit.is_some() && reader_done);
        if !receiving {
            if let Some(at) = wake {
                thread::sleep(at.saturating_duration_since(now));
            }
            continue;
        }
        let msg = wake.map_or_else(
            || rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
            |at| rx.recv_timeout(at.saturating_duration_since(now)),
        );
        match msg {
            Ok(Msg::Data(chunk)) => pending.extend_from_slice(&chunk),
            Ok(Msg::ReaderDone) => reader_done = true,
            Ok(Msg::Exit(code)) => shell_exit = Some((Instant::now(), code)),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Both senders are gone: nothing more can come.
                reader_done = true;
                shell_exit.get_or_insert((now, None));
            }
        }
    }
    emit.exit(id, shell_exit.and_then(|(_, code)| code));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Mutex};

    /// Timestamps every emit (each takes `delay`); remembers how many bytes
    /// were out at the exit.
    #[derive(Default)]
    struct Timed {
        delay: Duration,
        events: Mutex<Vec<(Instant, Vec<u8>)>>,
        exit: Mutex<Option<(Instant, Option<i32>, usize)>>,
    }

    impl Emit for Timed {
        fn data(&self, _id: &str, bytes: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push((Instant::now(), bytes.to_vec()));
            thread::sleep(self.delay);
        }

        fn exit(&self, _id: &str, code: Option<i32>) {
            let bytes = self.events.lock().unwrap().iter().map(|e| e.1.len()).sum();
            *self.exit.lock().unwrap() = Some((Instant::now(), code, bytes));
        }
    }

    impl Timed {
        fn sizes(&self) -> Vec<usize> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.1.len())
                .collect()
        }
    }

    #[test]
    fn a_burst_goes_out_in_spaced_events_of_at_most_64_kib() {
        const CHUNK: usize = 8 * 1024;
        let (tx, rx) = mpsc::sync_channel(64);
        // 25 chunks of a recognisable sequence: 200 KiB.
        let sent: Vec<u8> = (0..25 * CHUNK)
            .map(|i| u8::try_from(i % 251).unwrap())
            .collect();
        for chunk in sent.chunks(CHUNK) {
            tx.send(Msg::Data(chunk.to_vec())).unwrap();
        }
        tx.send(Msg::ReaderDone).unwrap();
        tx.send(Msg::Exit(Some(0))).unwrap();

        let out = Timed::default();
        pump(&rx, &out, "t", &Flags::new());

        // The first chunk after silence goes out at once; the rest is cut
        // into exact 64 KiB pieces, one per interval.
        assert_eq!(out.sizes(), [CHUNK, MAX_EVENT, MAX_EVENT, MAX_EVENT]);
        let events = out.events.lock().unwrap();
        for pair in events.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(gap >= MIN_INTERVAL, "events only {gap:?} apart");
        }
        let received: Vec<u8> = events.iter().flat_map(|e| e.1.clone()).collect();
        assert_eq!(received, sent, "bytes lost or reordered");
        drop(events);
        let exit = out.exit.lock().unwrap().unwrap();
        assert_eq!((exit.1, exit.2), (Some(0), sent.len()));
    }

    #[test]
    fn exit_waits_for_the_reader_but_not_for_ever() {
        let (tx, rx) = mpsc::sync_channel(8);
        tx.send(Msg::Exit(Some(1))).unwrap();
        let late = tx.clone();
        let reader = thread::spawn(move || {
            thread::sleep(Duration::from_millis(80));
            late.send(Msg::Data(b"late".to_vec())).unwrap();
            late.send(Msg::ReaderDone).unwrap();
        });
        let out = Timed::default();
        pump(&rx, &out, "t", &Flags::new());
        reader.join().unwrap();
        let exit = out.exit.lock().unwrap().unwrap();
        assert_eq!((exit.1, exit.2), (Some(1), 4), "late output not drained");

        // A reader that never finishes (`tx` stays alive) is abandoned.
        let out = Timed::default();
        tx.send(Msg::Data(b"x".to_vec())).unwrap();
        tx.send(Msg::Exit(None)).unwrap();
        let started = Instant::now();
        pump(&rx, &out, "t", &Flags::new());
        let waited = started.elapsed();
        assert!(
            (READER_GRACE..READER_GRACE * 3).contains(&waited),
            "waited {waited:?}"
        );
        assert_eq!(out.exit.lock().unwrap().unwrap().2, 1);
    }

    #[test]
    fn a_backlog_left_when_the_reader_is_abandoned_is_still_cut_into_capped_events() {
        const CHUNK: usize = 8 * 1024;
        let (tx, rx) = mpsc::sync_channel(64);
        tx.send(Msg::Exit(Some(0))).unwrap();
        for _ in 0..40 {
            tx.send(Msg::Data(vec![7; CHUNK])).unwrap();
        }
        // Slow emits: the grace period runs out with most of it still queued.
        let out = Timed {
            delay: Duration::from_millis(120),
            ..Timed::default()
        };
        pump(&rx, &out, "t", &Flags::new());

        let sizes = out.sizes();
        assert!(sizes.iter().all(|&s| s <= MAX_EVENT), "sizes: {sizes:?}");
        assert_eq!(sizes.iter().sum::<usize>(), 40 * CHUNK);
        drop(tx);
    }
}
