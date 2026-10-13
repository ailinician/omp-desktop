//! The pump: turns what the reader and waiter threads send into events, and
//! the [`Stream`] state it shares with the terminal's owner (output ring,
//! flow control, exit code).
//!
//! **Flow control.** The pump counts the bytes emitted but not yet acked by
//! the webview. At [`HIGH_WATER`] it stops emitting *and* stops draining its
//! channel, so the reader blocks on the full queue and the pty throttles the
//! shell; it resumes once acks bring the debt below [`LOW_WATER`], or a
//! replay resets it. A stalled pump waits on a condvar that `ack`, `replay`
//! and `close` notify.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use super::ring::{OutputRing, Replay};

/// Where terminal output goes. The app emits Tauri events; tests collect.
pub trait Emit: Send + Sync + 'static {
    /// One chunk of output of shell `instance`, the `seq`-th (from 1).
    fn data(&self, id: &str, instance: u64, seq: u64, bytes: &[u8]);
    /// Shell `instance` exited; `None` = killed by a signal.
    fn exit(&self, id: &str, instance: u64, code: Option<i32>);
}

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
/// Unacked bytes at which the pump stalls.
pub const HIGH_WATER: u64 = 512 * 1024;
/// A stalled pump resumes once the unacked bytes are below this.
pub const LOW_WATER: u64 = 128 * 1024;

/// What the terminal's threads tell the pump.
pub enum Msg {
    /// Output from the master.
    Data(Vec<u8>),
    /// The reader reached EOF or failed: no more data follows.
    ReaderDone,
    /// The shell exited and was reaped; `None` = killed by a signal.
    Exit(Option<i32>),
}

/// One terminal's output state, shared by its pump, its waiter and its
/// [`super::pty::Pty`].
pub struct Stream {
    flow: Mutex<Flow>,
    /// Notified by `ack`, `replay` and `close`; a stalled pump waits on it.
    wake: Condvar,
    /// Set once by the waiter, before it tells the pump: `None` = signal.
    exit: OnceLock<Option<i32>>,
}

/// Guarded by one lock so that a replay copies exactly the chunks whose
/// debt it forgives.
struct Flow {
    /// Owner gone (closed, replaced, shut down): emit nothing any more.
    closed: bool,
    /// Bytes emitted minus bytes acked (saturating).
    unacked: u64,
    ring: OutputRing,
}

/// May the pump go on?
enum Credit {
    /// Yes, it was not stalled.
    Ready,
    /// Yes, after a stall.
    Resumed,
    /// No: the terminal is closed.
    Closed,
}

impl Stream {
    pub const fn new() -> Self {
        Self {
            flow: Mutex::new(Flow {
                closed: false,
                unacked: 0,
                ring: OutputRing::new(),
            }),
            wake: Condvar::new(),
            exit: OnceLock::new(),
        }
    }

    fn flow(&self) -> MutexGuard<'_, Flow> {
        self.flow.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `None` while the shell runs; `Some(code)` once it exited (`code`
    /// `None` = killed by a signal).
    // The terminal contract's shape (`Pty::exit_code`): "running" and
    // "killed by a signal" are both states the manager reports.
    #[allow(clippy::option_option)]
    pub fn exit_code(&self) -> Option<Option<i32>> {
        self.exit.get().copied()
    }

    /// Record the shell's exit code (the first one set wins).
    pub fn set_exit(&self, code: Option<i32>) {
        let _ = self.exit.set(code);
    }

    /// The webview has rendered `bytes` more of the emitted output.
    pub fn ack(&self, bytes: u64) {
        let mut flow = self.flow();
        flow.unacked = flow.unacked.saturating_sub(bytes);
        drop(flow);
        self.wake.notify_all();
    }

    /// The held output after `after_seq`; forgives the whole debt, since the
    /// caller renders the replay instead of acking the events it covers.
    pub fn replay(&self, after_seq: u64) -> Replay {
        let mut flow = self.flow();
        let replay = flow.ring.replay(after_seq);
        flow.unacked = 0;
        drop(flow);
        self.wake.notify_all();
        replay
    }

    /// Silence the terminal for good: no event is emitted after this.
    pub fn close(&self) {
        self.flow().closed = true;
        self.wake.notify_all();
    }

    fn is_closed(&self) -> bool {
        self.flow().closed
    }

    /// Block while the debt is at [`HIGH_WATER`] until it drops below
    /// [`LOW_WATER`] (or is forgiven), or the terminal is closed.
    fn credit(&self) -> Credit {
        let flow = self.flow();
        let (closed, stalled) = (flow.closed, flow.unacked >= HIGH_WATER);
        if closed || !stalled {
            drop(flow);
            return if closed {
                Credit::Closed
            } else {
                Credit::Ready
            };
        }
        let closed = self
            .wake
            .wait_while(flow, |f| !f.closed && f.unacked >= LOW_WATER)
            .unwrap_or_else(PoisonError::into_inner)
            .closed;
        if closed {
            Credit::Closed
        } else {
            Credit::Resumed
        }
    }

    /// Append `chunk` to the ring and charge it to the debt; its seq, or
    /// `None` when the terminal is closed. Into the ring before the event
    /// goes out, so a replay racing the event covers it (the webview drops
    /// the event by its seq) rather than missing it.
    fn record(&self, chunk: Vec<u8>) -> Option<(u64, Arc<Vec<u8>>)> {
        let chunk = Arc::new(chunk);
        let mut flow = self.flow();
        if flow.closed {
            return None;
        }
        flow.unacked += chunk.len() as u64;
        let seq = flow.ring.push(Arc::clone(&chunk));
        drop(flow);
        Some((seq, chunk))
    }
}

/// Emit the output as events: one per [`MIN_INTERVAL`] at the most (the
/// first after a quiet spell goes out at once), each of at most
/// [`MAX_EVENT`] bytes — a larger backlog is emitted piecewise, one piece
/// per interval, and holds the reader back meanwhile — and none while the
/// webview owes [`HIGH_WATER`] acks (see the module docs). Then the exit
/// event, once the shell is reaped, the reader is done (or abandoned after
/// [`READER_GRACE`] of unstalled time) and everything is out. A closed
/// terminal emits nothing and returns promptly, also while stalled.
pub fn pump<E: Emit>(rx: &Receiver<Msg>, emit: &E, id: &str, instance: u64, stream: &Stream) {
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
            emit.data(id, instance, seq, &chunk);
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
        emit.exit(id, instance, shell_exit.and_then(|(_, code)| code));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    /// One data event as the emitter saw it: when, instance, seq, bytes.
    type Event = (Instant, u64, u64, Vec<u8>);

    /// Timestamps every emit (each takes `delay`); remembers how many bytes
    /// were out at the exit. With `auto_ack`, acks each event at once.
    #[derive(Default)]
    struct Timed {
        delay: Duration,
        auto_ack: Option<Arc<Stream>>,
        events: Mutex<Vec<Event>>,
        exit: Mutex<Option<(Instant, Option<i32>, usize)>>,
    }

    impl Emit for Timed {
        fn data(&self, _id: &str, instance: u64, seq: u64, bytes: &[u8]) {
            self.events
                .lock()
                .unwrap()
                .push((Instant::now(), instance, seq, bytes.to_vec()));
            if let Some(stream) = &self.auto_ack {
                stream.ack(bytes.len() as u64);
            }
            thread::sleep(self.delay);
        }

        fn exit(&self, _id: &str, _instance: u64, code: Option<i32>) {
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
                .map(|e| e.3.len())
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
        pump(rx, out, "t", 1, &Stream::new());
    }

    /// Poll `check` until it holds or `limit` passes.
    fn wait_until(limit: Duration, check: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < limit {
            if check() {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        check()
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

        // The first chunk after silence goes out at once; the rest is cut
        // into exact 64 KiB pieces, one per interval.
        assert_eq!(out.sizes(), [CHUNK, MAX_EVENT, MAX_EVENT, MAX_EVENT]);
        let events = out.events.lock().unwrap();
        for pair in events.windows(2) {
            let gap = pair[1].0 - pair[0].0;
            assert!(gap >= MIN_INTERVAL, "events only {gap:?} apart");
        }
        let received: Vec<u8> = events.iter().flat_map(|e| e.3.iter().copied()).collect();
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
    fn events_carry_the_instance_and_seqs_from_one_and_land_in_the_ring() {
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
        pump(&rx, &out, "t", 7, &stream);
        feeder.join().unwrap();

        let events = out.events.lock().unwrap();
        let tags: Vec<(u64, u64)> = events.iter().map(|e| (e.1, e.2)).collect();
        assert_eq!(tags, [(7, 1), (7, 2), (7, 3)]);
        drop(events);
        let replay = stream.replay(1);
        assert_eq!(
            (replay.bytes.as_slice(), replay.head_seq, replay.dropped),
            (&b"twothree"[..], 3, false)
        );

        // A new instance has its own stream: seqs start over.
        let (tx, rx) = mpsc::sync_channel(8);
        tx.send(Msg::Data(b"again".to_vec())).unwrap();
        tx.send(Msg::ReaderDone).unwrap();
        tx.send(Msg::Exit(Some(0))).unwrap();
        let out = Timed::default();
        pump(&rx, &out, "t", 8, &Stream::new());
        let tag = out.events.lock().unwrap().first().map(|e| (e.1, e.2));
        assert_eq!(tag, Some((8, 1)));
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
        pump(&rx, &out, "t", 1, &stream);
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
        let handle = thread::spawn(move || pump(&rx, pump_out.as_ref(), "t", 1, &pump_stream));
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
