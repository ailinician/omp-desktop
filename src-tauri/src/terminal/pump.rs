//! The pump: turns what the reader and waiter threads send into events,
//! paced, capped and under the flow control of [`Stream`] (see
//! [`super::stream`]).

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use super::stream::{Credit, Stream};

/// Where terminal output goes: the app emits Tauri events, tests collect.
/// Handed to `TerminalManager::open`, which turns it into the emitter of the
/// shell it spawns (and drops it when it spawns nothing).
pub trait Emit {
    /// The emitter one shell's pump owns.
    type Terminal: TerminalEmit + Send + 'static;
    /// The emitter of shell `instance` of tab `id`, built once per shell so
    /// the pump's per-event work allocates nothing it could reuse.
    fn terminal(self, id: &str, instance: u64) -> Self::Terminal;
}

/// One shell's events, owned by its pump.
pub trait TerminalEmit {
    /// The `seq`-th chunk of output (from 1).
    fn data(&mut self, seq: u64, bytes: &[u8]);
    /// The shell exited; `None` = killed by a signal.
    fn exit(&mut self, code: Option<i32>);
}

/// Largest payload of one event.
pub const MAX_EVENT: usize = 64 * 1024;
/// Minimum spacing between two events of one terminal.
pub const MIN_INTERVAL: Duration = Duration::from_millis(4);
/// How long after the shell exited the reader may take to reach EOF. macOS
/// hangs the pty up when its session leader exits, so EOF follows at once.
/// Linux does not: a background job holding the slave open keeps the reader
/// from EOF for as long as it lives. The pump abandons the reader after
/// this, dropping its late output, so the exit event is never held up.
pub const READER_GRACE: Duration = Duration::from_millis(250);

/// What the terminal's threads tell the pump.
pub enum Msg {
    /// Output from the master.
    Data(Vec<u8>),
    /// The reader reached EOF or failed: no more data follows.
    ReaderDone,
    /// The shell exited (unix: observed with `waitid(WNOWAIT)`, the leader
    /// is not reaped yet; Windows: reaped); `None` = killed by a signal.
    Exit(Option<i32>),
}

/// Emit the output as events: one per [`MIN_INTERVAL`] at the most (the
/// first after a quiet spell goes out at once), each of at most
/// [`MAX_EVENT`] bytes — a larger backlog is emitted piecewise, one piece
/// per interval, and holds the reader back meanwhile — and none while the
/// webview owes [`super::stream::HIGH_WATER`] acks. Then the exit event,
/// once the shell's exit was observed, the reader is done (or abandoned
/// after [`READER_GRACE`] of unstalled time) and everything is out. A closed
/// terminal emits nothing and returns promptly, also while stalled.
pub fn pump(rx: &Receiver<Msg>, mut emit: impl TerminalEmit, stream: &Stream) {
    let mut pending: Vec<u8> = Vec::new();
    let mut last_emit: Option<Instant> = None;
    let mut reader_done = false;
    let mut shell_exit: Option<(Instant, Option<i32>)> = None;
    loop {
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
        match stream.credit() {
            Credit::Closed => return,
            Credit::Resumed => {
                // The reader was held back by the stall, not slow: give it
                // its full grace from now.
                if let Some((at, _)) = shell_exit.as_mut() {
                    *at = Instant::now();
                }
                continue;
            }
            Credit::Ready => {}
        }
        let emit_at = (!pending.is_empty()).then(|| last_emit.map_or(now, |t| t + MIN_INTERVAL));
        if emit_at.is_some_and(|t| t <= now) {
            let rest = pending.split_off(pending.len().min(MAX_EVENT));
            let chunk = std::mem::replace(&mut pending, rest);
            let Some((seq, chunk)) = stream.record(chunk) else {
                return;
            };
            emit.data(seq, &chunk);
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
                stream.set_exit(None);
            }
        }
    }
    if !stream.is_closed() {
        emit.exit(shell_exit.and_then(|(_, code)| code));
    }
}

#[cfg(test)]
mod tests {
    use super::super::stream::{HIGH_WATER, LOW_WATER};
    use super::super::test_support::wait_until;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{mpsc, Arc, Mutex};

    /// One data event as the emitter saw it: when, seq, bytes.
    type Event = (Instant, u64, Vec<u8>);

    /// Timestamps every emit (each takes `delay`); remembers when the exit
    /// came and how many bytes were out by then. With `auto_ack`, acks each
    /// event at once.
    #[derive(Default)]
    struct Timed {
        delay: Duration,
        auto_ack: Option<Arc<Stream>>,
        events: Mutex<Vec<Event>>,
        exit: Mutex<Option<(Instant, Option<i32>, usize)>>,
    }

    impl TerminalEmit for &Timed {
        fn data(&mut self, seq: u64, bytes: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push((Instant::now(), seq, bytes.to_vec()));
            if let Some(stream) = &self.auto_ack {
                stream.ack(bytes.len() as u64);
            }
            thread::sleep(self.delay);
        }

        fn exit(&mut self, code: Option<i32>) {
            let bytes = self.emitted();
            *self.exit.lock().unwrap() = Some((Instant::now(), code, bytes));
        }
    }

    impl Timed {
        fn sizes(&self) -> Vec<usize> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .map(|e| e.2.len())
                .collect()
        }

        fn emitted(&self) -> usize {
            self.sizes().iter().sum()
        }

        fn exit_bytes(&self) -> Option<usize> {
            self.exit.lock().unwrap().map(|e| e.2)
        }
    }

    fn run(rx: &Receiver<Msg>, out: &Timed) {
        pump(rx, out, &Stream::new());
    }

    /// `check` turned nonzero and keeps holding the same value for a while.
    fn settled(check: impl Fn() -> usize) -> usize {
        assert!(
            wait_until(Duration::from_secs(5), || check() > 0),
            "never started"
        );
        let mut last = check();
        loop {
            thread::sleep(Duration::from_millis(60));
            let now = check();
            if now == last {
                return now;
            }
            last = now;
        }
    }

    /// The pacing contract, whatever the scheduling: every event at most
    /// [`MAX_EVENT`], events at least [`MIN_INTERVAL`] apart, the bytes
    /// complete and in order, the exit last.
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
        run(&rx, &out);

        let events = out.events.lock().unwrap();
        for (_, _, bytes) in events.iter() {
            assert!((1..=MAX_EVENT).contains(&bytes.len()), "{}", bytes.len());
        }
        for pair in events.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(gap >= MIN_INTERVAL, "events only {gap:?} apart");
        }
        let received: Vec<u8> = events.iter().flat_map(|e| e.2.iter().copied()).collect();
        assert_eq!(received, sent, "bytes lost or reordered");
        let last = events.last().map(|e| e.0);
        drop(events);
        let (at, code, bytes) = out.exit.lock().unwrap().unwrap();
        assert_eq!((code, bytes), (Some(0), sent.len()), "exit before the data");
        assert!(last.is_some_and(|last| at >= last));
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
        run(&rx, &out);
        reader.join().unwrap();
        let exit = out.exit.lock().unwrap().unwrap();
        assert_eq!((exit.1, exit.2), (Some(1), 4), "late output not drained");

        // A reader that never finishes (`tx` stays alive) is abandoned.
        let out = Timed::default();
        tx.send(Msg::Data(b"x".to_vec())).unwrap();
        tx.send(Msg::Exit(None)).unwrap();
        let started = Instant::now();
        run(&rx, &out);
        let waited = started.elapsed();
        assert!(
            (READER_GRACE..READER_GRACE * 3).contains(&waited),
            "waited {waited:?}"
        );
        assert_eq!(out.exit_bytes(), Some(1));
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
        run(&rx, &out);

        let sizes = out.sizes();
        assert!(sizes.iter().all(|&s| s <= MAX_EVENT), "sizes: {sizes:?}");
        assert_eq!(sizes.iter().sum::<usize>(), 40 * CHUNK);
        drop(tx);
    }

    #[test]
    fn events_carry_seqs_from_one_and_land_in_the_ring() {
        let (tx, rx) = mpsc::sync_channel(8);
        let stream = Stream::new();
        let out = Timed::default();
        let feeder = thread::spawn(move || {
            for part in [&b"one"[..], b"two", b"three"] {
                tx.send(Msg::Data(part.to_vec())).unwrap();
                // Past the interval: one event per part.
                thread::sleep(MIN_INTERVAL * 5);
            }
            tx.send(Msg::ReaderDone).unwrap();
            tx.send(Msg::Exit(Some(0))).unwrap();
        });
        pump(&rx, &out, &stream);
        feeder.join().unwrap();

        let events = out.events.lock().unwrap();
        let seqs: Vec<u64> = events.iter().map(|e| e.1).collect();
        assert_eq!(seqs, [1, 2, 3]);
        drop(events);
        let replay = stream.replay(1);
        assert_eq!(
            (replay.bytes.as_slice(), replay.head_seq, replay.dropped),
            (&b"twothree"[..], 3, false)
        );

        // A new shell has its own stream: seqs start over.
        let (tx, rx) = mpsc::sync_channel(8);
        tx.send(Msg::Data(b"again".to_vec())).unwrap();
        tx.send(Msg::ReaderDone).unwrap();
        tx.send(Msg::Exit(Some(0))).unwrap();
        let out = Timed::default();
        pump(&rx, &out, &Stream::new());
        let seq = out.events.lock().unwrap().first().map(|e| e.1);
        assert_eq!(seq, Some(1));
    }

    #[test]
    fn exit_follows_the_last_data_under_flow_control() {
        const CHUNK: usize = 8 * 1024;
        const TOTAL: usize = 160; // 1.25 MiB, well past HIGH_WATER
        let (tx, rx) = mpsc::sync_channel(64);
        let stream = Arc::new(Stream::new());
        let out = Timed {
            auto_ack: Some(Arc::clone(&stream)),
            ..Timed::default()
        };
        let feeder = thread::spawn(move || {
            for _ in 0..TOTAL {
                tx.send(Msg::Data(vec![1; CHUNK])).unwrap();
            }
            tx.send(Msg::ReaderDone).unwrap();
            tx.send(Msg::Exit(Some(3))).unwrap();
        });
        pump(&rx, &out, &stream);
        feeder.join().unwrap();
        let exit = out.exit.lock().unwrap().unwrap();
        assert_eq!((exit.1, exit.2), (Some(3), TOTAL * CHUNK));
    }

    /// A pump fed far more than [`HIGH_WATER`] with nobody acking: returns
    /// the stream, the emitter, the count of chunks the feeder got into the
    /// channel, and the pump thread.
    fn stalled_pump() -> (
        Arc<Stream>,
        Arc<Timed>,
        Arc<AtomicUsize>,
        thread::JoinHandle<()>,
    ) {
        const CHUNK: usize = 8 * 1024;
        let (tx, rx) = mpsc::sync_channel(4);
        let stream = Arc::new(Stream::new());
        let out = Arc::new(Timed::default());
        let fed = Arc::new(AtomicUsize::new(0));
        let feeder_fed = Arc::clone(&fed);
        thread::spawn(move || {
            // Ends once the pump is gone (send fails).
            while tx.send(Msg::Data(vec![2; CHUNK])).is_ok() {
                feeder_fed.fetch_add(1, Ordering::SeqCst);
            }
        });
        let pump_stream = Arc::clone(&stream);
        let pump_out = Arc::clone(&out);
        let handle = thread::spawn(move || pump(&rx, pump_out.as_ref(), &pump_stream));
        (stream, out, fed, handle)
    }

    fn high() -> usize {
        usize::try_from(HIGH_WATER).unwrap()
    }

    #[test]
    fn a_pump_stops_at_high_water_and_resumes_below_low_water() {
        let (stream, out, fed, handle) = stalled_pump();

        let stalled_at = settled(|| out.emitted());
        assert!(
            (high()..=high() + MAX_EVENT).contains(&stalled_at),
            "stalled at {stalled_at} unacked"
        );
        // Not draining the channel either: the feeder is blocked.
        let fed_at_stall = settled(|| fed.load(Ordering::SeqCst));
        thread::sleep(Duration::from_millis(50));
        assert_eq!(
            fed.load(Ordering::SeqCst),
            fed_at_stall,
            "channel still drained"
        );

        // Down to exactly LOW_WATER is not enough.
        let at_low = stalled_at as u64 - LOW_WATER;
        stream.ack(at_low);
        thread::sleep(Duration::from_millis(60));
        assert_eq!(out.emitted(), stalled_at, "resumed at LOW_WATER");

        // One byte more and it runs again, up to the next stall.
        stream.ack(1);
        let next = settled(|| out.emitted());
        assert!(next > stalled_at, "did not resume");
        let unacked = next - stalled_at + usize::try_from(LOW_WATER).unwrap() - 1;
        assert!(
            (high()..=high() + MAX_EVENT).contains(&unacked),
            "stalled again at {unacked} unacked"
        );

        stream.close();
        handle.join().unwrap();
    }

    #[test]
    fn a_replay_forgives_the_debt() {
        let (stream, out, _fed, handle) = stalled_pump();
        let stalled_at = settled(|| out.emitted());
        let replay = stream.replay(0);
        assert_eq!(replay.bytes.len(), stalled_at, "ring lags the events");
        let next = settled(|| out.emitted());
        assert!(
            (stalled_at + high()..=stalled_at + high() + MAX_EVENT).contains(&next),
            "after the replay: {next} emitted, stalled at {stalled_at}"
        );
        stream.close();
        handle.join().unwrap();
    }

    #[test]
    fn a_closed_pump_exits_promptly_while_stalled() {
        let (stream, out, _fed, handle) = stalled_pump();
        settled(|| out.emitted());
        let closed = Instant::now();
        stream.close();
        assert!(
            wait_until(Duration::from_millis(500), || handle.is_finished()),
            "pump still waiting"
        );
        assert!(closed.elapsed() < Duration::from_millis(500));
        handle.join().unwrap();
        assert_eq!(out.exit_bytes(), None, "a closed terminal emitted its exit");
    }
}
