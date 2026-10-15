//! What the splice hands the [`Tap`]: complete CDP messages, decoded on the
//! pipe threads and observed on a thread of their own.
//!
//! Each pipe thread owns the [`Feed`] of its direction and calls
//! [`Feed::push`] on every chunk *before* forwarding it. `push` only runs the
//! framing ([`Messages`]; for the server, after checking the upgrade
//! response) and queues each text message that completes and fits the size
//! cap; a bigger payload streams past without being copied. Both directions
//! share one queue, so a command is queued before its last byte can reach
//! Chromium and cause a reply, and the observer meets commands, replies and
//! the commands depending on them in causal order.
//!
//! Parsing JSON and the follow callback run on the observer thread only, and
//! the splice never waits for it: a full queue, broken framing or a
//! negotiated extension switches the tap off for the connection (both sides
//! and the observer), never the splice.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread;

use super::tap::Tap;
use super::ws_frames::{Lost, Messages};

/// Largest command the tap reads (a client message over it is skipped).
const MAX_COMMAND: usize = 1 << 20;
/// Largest reply or event the tap reads. Everything it needs is small; a
/// screenshot's reply is not, and is skipped without being buffered.
const MAX_REPLY: usize = 64 << 10;
/// Longest response head of the upgrade.
const MAX_UPGRADE_HEAD: usize = 16 << 10;
/// Messages queued for the observer of one connection, at most.
const QUEUE_MESSAGES: usize = 1024;
/// Bytes queued for the observer of one connection, at most: room for four
/// of the largest commands the tap reads, or sixty-four of the largest
/// replies. Skipped payloads never count, so a burst of screenshots costs
/// nothing here; only an observer stuck for good fills it.
const QUEUE_BYTES: usize = 4 * MAX_COMMAND;

/// Which way a message went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    /// Sent by the client (the agent).
    Client,
    /// Sent by the server (Chromium).
    Server,
}

type Queued = (Direction, Vec<u8>);

/// What both feeds and the observer of a connection share.
#[derive(Default)]
struct State {
    /// Why the tap was switched off, once it was.
    off: OnceLock<&'static str>,
    /// Payload bytes in the queue.
    queued: AtomicUsize,
}

impl State {
    /// Switch the tap off for the connection and say why, once.
    fn off(&self, why: &'static str) {
        if self.off.set(why).is_ok() {
            eprintln!("[omp-desktop] browser tap off for this connection: {why}");
        }
    }

    fn is_off(&self) -> bool {
        self.off.get().is_some()
    }
}

enum Decoder {
    /// The server's response head so far.
    Upgrade(Vec<u8>),
    Frames(Messages),
}

/// One direction's end of a connection's observer, owned by its pipe thread.
pub(super) struct Feed {
    direction: Direction,
    decoder: Decoder,
    queue: SyncSender<Queued>,
    state: Arc<State>,
    /// How long this feed sleeps in every `push` (the ordering test slows
    /// the client side of its own proxy).
    #[cfg(test)]
    delay: std::time::Duration,
}

/// Start the observer thread that owns `tap`; returns the client's and the
/// server's feed. The thread ends when both are gone. If it cannot start,
/// the feeds are born off: the splice goes on untapped.
pub(super) fn spawn(tap: Tap) -> (Feed, Feed) {
    let (client, server, messages, state) = channel();
    let observer = Arc::clone(&state);
    let started = thread::Builder::new()
        .name("browser-proxy-tap".into())
        .spawn(move || observe(tap, &messages, &observer));
    if let Err(e) = started {
        eprintln!("[omp-desktop] browser tap: observer thread: {e}");
        state.off("no observer thread");
    }
    (client, server)
}

/// Both feeds of a connection and the receiving end of their queue.
fn channel() -> (Feed, Feed, Receiver<Queued>, Arc<State>) {
    let (queue, messages) = mpsc::sync_channel(QUEUE_MESSAGES);
    let state = Arc::new(State::default());
    let feed = |direction, decoder, queue| Feed {
        direction,
        decoder,
        queue,
        state: Arc::clone(&state),
        #[cfg(test)]
        delay: std::time::Duration::ZERO,
    };
    let client = feed(
        Direction::Client,
        Decoder::Frames(Messages::new(true, MAX_COMMAND)),
        queue.clone(),
    );
    let server = feed(Direction::Server, Decoder::Upgrade(Vec::new()), queue);
    (client, server, messages, state)
}

impl Feed {
    /// Decode `bytes`, the next chunk of this direction, and queue the text
    /// messages it completes. Never blocks: the pipe thread calls it before
    /// it forwards the chunk.
    pub(super) fn push(&mut self, bytes: &[u8]) {
        if bytes.is_empty() || self.state.is_off() {
            return;
        }
        #[cfg(test)]
        thread::sleep(self.delay);
        let mut messages = Vec::new();
        if let Err(why) = self.decode(bytes, &mut messages) {
            self.state.off(why);
            return;
        }
        for message in messages {
            if let Err(why) = self.enqueue(message) {
                self.state.off(why);
                return;
            }
        }
    }

    fn decode(&mut self, bytes: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), &'static str> {
        let lost = match self.direction {
            Direction::Client => "client frames cannot be followed",
            Direction::Server => "server frames cannot be followed",
        };
        let frames = match &mut self.decoder {
            Decoder::Frames(messages) => return messages.feed(bytes, out).map_err(|Lost| lost),
            Decoder::Upgrade(head) => {
                head.extend_from_slice(bytes);
                let Some(end) = head.windows(4).position(|w| w == b"\r\n\r\n") else {
                    return if head.len() > MAX_UPGRADE_HEAD {
                        Err("no upgrade response")
                    } else {
                        Ok(())
                    };
                };
                check_upgrade(&head[..end])?;
                // The frames that rode in with the head.
                head.split_off(end + 4)
            }
        };
        let mut messages = Messages::new(false, MAX_REPLY);
        let fed = messages.feed(&frames, out).map_err(|Lost| lost);
        self.decoder = Decoder::Frames(messages);
        fed
    }

    fn enqueue(&self, message: Vec<u8>) -> Result<(), &'static str> {
        let len = message.len();
        if self.state.queued.fetch_add(len, Ordering::AcqRel) + len > QUEUE_BYTES {
            return Err("observer queue full");
        }
        match self.queue.try_send((self.direction, message)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("observer queue full"),
            Err(TrySendError::Disconnected(_)) => Err("observer stopped"),
        }
    }

    /// `#[cfg(test)]`: this feed, sleeping `millis` in every push.
    #[cfg(test)]
    pub(super) const fn with_delay_ms(mut self, millis: u64) -> Self {
        self.delay = std::time::Duration::from_millis(millis);
        self
    }

    /// Why the tap of this connection is off, if it is.
    #[cfg(test)]
    pub(super) fn off_reason(&self) -> Option<&'static str> {
        self.state.off.get().copied()
    }
}

/// The observer thread: hands the messages to the tap in queue order. A
/// panic in it (or in the callback) switches the tap off without affecting
/// forwarding.
fn observe(mut tap: Tap, messages: &Receiver<Queued>, state: &State) {
    for (direction, message) in messages {
        state.queued.fetch_sub(message.len(), Ordering::AcqRel);
        if state.is_off() {
            return;
        }
        match catch_unwind(AssertUnwindSafe(|| tap.on_message(direction, &message))) {
            Ok(Ok(())) => {}
            Ok(Err(why)) => return state.off(why),
            Err(_) => return state.off("internal error"),
        }
    }
}

/// `Ok` for the `101` response head of a WebSocket upgrade without a
/// negotiated extension (compressed frames cannot be read).
fn check_upgrade(head: &[u8]) -> Result<(), &'static str> {
    let head = std::str::from_utf8(head).map_err(|_| "upgrade response is not text")?;
    let mut lines = head.split("\r\n");
    if lines.next().and_then(|s| s.split_whitespace().nth(1)) != Some("101") {
        return Err("upgrade refused");
    }
    let extended = lines.any(|line| {
        line.split_once(':')
            .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("sec-websocket-extensions"))
    });
    if extended {
        return Err("Sec-WebSocket-Extensions negotiated (compressed frames)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::super::tests::{frame, masked_text, text, Reported};
    use super::super::{tap::Tap, AgentTarget};
    use super::{channel, spawn, Direction, Feed, Queued, MAX_REPLY, QUEUE_MESSAGES};
    use crate::browser::lock;
    use crate::browser::tests::eventually;

    const BROWSER: &[u8] = b"GET /devtools/browser/x HTTP/1.1\r\nHost: h\r\n\r\n";
    const UPGRADED: &[u8] = b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";

    /// What the feeds queued so far, in queue order.
    fn drained(queue: &Receiver<Queued>) -> Vec<(Direction, String)> {
        queue
            .try_iter()
            .map(|(direction, m)| (direction, String::from_utf8(m).unwrap()))
            .collect()
    }

    fn client(message: &str) -> (Direction, String) {
        (Direction::Client, message.to_owned())
    }

    fn server(message: &str) -> (Direction, String) {
        (Direction::Server, message.to_owned())
    }

    /// Pushes `wire` to `feed` in chunks of `step` bytes, as the pipe does.
    fn push_in_chunks(feed: &mut Feed, wire: &[u8], step: usize) {
        for chunk in wire.chunks(step) {
            feed.push(chunk);
        }
    }

    #[test]
    fn fragments_control_frames_and_one_byte_reads_make_no_difference() {
        let message = r#"{"id":1,"method":"Page.navigate"}"#;
        let (head, tail) = message.as_bytes().split_at(10);
        let mut wire = frame(0x1, false, true, head);
        wire.extend(frame(0x9, true, true, b"hi"));
        wire.extend(frame(0x0, true, true, tail));
        for step in [1, 3, wire.len()] {
            let (mut up, _down, queue, _) = channel();
            push_in_chunks(&mut up, &wire, step);
            assert_eq!(drained(&queue), [client(message)], "step {step}");
        }
    }

    #[test]
    fn oversized_payloads_are_never_queued_and_decoding_stays_in_sync() {
        let (mut up, mut down, queue, state) = channel();
        down.push(UPGRADED);
        // A screenshot-sized reply streamed past in splice-sized reads.
        push_in_chunks(&mut down, &text(&"x".repeat(5 << 20)), 16 << 10);
        down.push(&text("after"));
        // A command over its cap, then one inside it.
        up.push(&masked_text(&"y".repeat(1_500_000)));
        up.push(&masked_text("next"));
        assert_eq!(drained(&queue), [server("after"), client("next")]);
        assert_eq!(
            state.queued.load(Ordering::Acquire),
            "after".len() + "next".len()
        );
        assert_eq!(down.off_reason(), None);
    }

    #[test]
    fn commands_and_replies_are_queued_in_the_order_they_were_pushed() {
        let (mut up, mut down, queue, _) = channel();
        down.push(UPGRADED);
        up.push(&masked_text("c1"));
        down.push(&text("r1"));
        down.push(&text("e1"));
        up.push(&masked_text("c2"));
        down.push(&text("r2"));
        assert_eq!(
            drained(&queue),
            [
                client("c1"),
                server("r1"),
                server("e1"),
                client("c2"),
                server("r2")
            ]
        );
    }

    #[test]
    fn the_upgrade_response_is_skipped_and_checked() {
        for step in [7, 4096] {
            let (_up, mut down, queue, _) = channel();
            let mut wire = UPGRADED.to_vec();
            wire.extend(text("behind the head"));
            push_in_chunks(&mut down, &wire, step);
            assert_eq!(drained(&queue), [server("behind the head")], "step {step}");
        }
        for (response, why) in [
            (
                "HTTP/1.1 101 Switching Protocols\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n",
                "Sec-WebSocket-Extensions negotiated (compressed frames)",
            ),
            (
                "HTTP/1.1 101 Switching Protocols\r\nsec-websocket-EXTENSIONS: x\r\n\r\n",
                "Sec-WebSocket-Extensions negotiated (compressed frames)",
            ),
            (
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n",
                "upgrade refused",
            ),
        ] {
            let (mut up, mut down, queue, _) = channel();
            assert_eq!(down.off_reason(), None);
            down.push(response.as_bytes());
            down.push(&text("ignored"));
            up.push(&masked_text("ignored"));
            assert_eq!(down.off_reason(), Some(why), "{response}");
            assert!(drained(&queue).is_empty(), "{response}");
        }
    }

    #[test]
    fn a_response_that_never_ends_switches_the_tap_off() {
        let (_up, mut down, queue, _) = channel();
        down.push(b"HTTP/1.1 101 Switching Protocols\r\n");
        down.push(&vec![b'a'; 20_000]);
        down.push(UPGRADED); // too late
        down.push(&text("ignored"));
        assert_eq!(down.off_reason(), Some("no upgrade response"));
        assert_eq!(drained(&queue), Vec::new());
    }

    #[test]
    fn garbage_switches_the_tap_off_and_never_panics() {
        let (mut up, mut down, queue, _) = channel();
        down.push(UPGRADED);
        up.push(&[0xFF; 32]);
        up.push(&masked_text("ignored"));
        down.push(&text("ignored"));
        assert_eq!(up.off_reason(), Some("client frames cannot be followed"));
        assert!(drained(&queue).is_empty(), "off after the first violation");

        // Pseudo-random bytes, in odd chunks, both directions, with and
        // without a valid upgrade first.
        let mut state = 0x2545_F491_4F6C_DD1D_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..300 {
            let (mut up, mut down, queue, _) = channel();
            if round % 2 == 0 {
                down.push(UPGRADED);
            }
            for _ in 0..20 {
                let len = usize::try_from(next() % 40).unwrap();
                let bytes: Vec<u8> = (0..len).map(|_| next().to_le_bytes()[0]).collect();
                if next() % 2 == 0 {
                    up.push(&bytes);
                } else {
                    down.push(&bytes);
                }
            }
            drop(drained(&queue));
        }
    }

    fn activate(target: &str) -> Vec<u8> {
        masked_text(
            &json!({ "id": 1, "method": "Target.activateTarget", "params": { "targetId": target } })
                .to_string(),
        )
    }

    /// A tap whose callback blocks on `gate` and counts its calls.
    fn gated_tap(gate: &Arc<Mutex<()>>, reported: &Reported) -> (Tap, Arc<AtomicUsize>) {
        let stuck = Arc::clone(gate);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let report = reported.callback();
        let callback: AgentTarget = Arc::new(move |target| {
            counted.fetch_add(1, Ordering::SeqCst);
            drop(lock(&stuck));
            report(target);
        });
        (Tap::new(BROWSER, callback), calls)
    }

    #[test]
    fn a_large_reply_behind_a_stuck_observer_leaves_the_tap_on() {
        let gate = Arc::new(Mutex::new(()));
        let reported = Reported::default();
        let (tap, calls) = gated_tap(&gate, &reported);
        let held = lock(&gate);
        let (mut up, mut down) = spawn(tap);
        down.push(UPGRADED);
        up.push(&activate("T1")); // the observer blocks in this callback
        assert!(
            eventually(Duration::from_secs(3), || {
                (calls.load(Ordering::SeqCst) == 1).then_some(())
            })
            .is_some(),
            "the observer reached the callback"
        );
        // Screenshots far bigger than any queue, and one reply of the largest
        // size the tap reads, all while the observer is stuck.
        for _ in 0..3 {
            push_in_chunks(&mut down, &text(&"x".repeat(8 << 20)), 16 << 10);
        }
        down.push(&text(&format!("\"{}\"", "y".repeat(MAX_REPLY - 2))));
        up.push(&activate("T2"));
        assert_eq!(up.off_reason(), None, "nothing overflowed");
        drop(held);
        assert!(
            eventually(Duration::from_secs(3), || (reported.all().len() == 2)
                .then_some(()))
            .is_some(),
            "{:?}",
            reported.all()
        );
        assert_eq!(reported.all(), ["T1", "T2"]);
    }

    #[test]
    fn a_stuck_observer_disables_the_tap_without_blocking_the_splice() {
        let gate = Arc::new(Mutex::new(()));
        let reported = Reported::default();
        let (tap, calls) = gated_tap(&gate, &reported);
        let held = lock(&gate);
        let (mut up, mut down) = spawn(tap);
        down.push(UPGRADED);
        up.push(&activate("T")); // the observer blocks in this callback
        assert!(eventually(Duration::from_secs(3), || {
            (calls.load(Ordering::SeqCst) == 1).then_some(())
        })
        .is_some());
        let start = Instant::now();
        for _ in 0..=QUEUE_MESSAGES {
            up.push(&activate("T"));
        }
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "push must not block"
        );
        assert_eq!(up.off_reason(), Some("observer queue full"));
        drop(held); // the callback may finish; the tap must not report any more
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "no callback after the overflow point"
        );
    }

    #[test]
    fn a_panicking_callback_switches_the_tap_off_instead_of_the_splice() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let callback: AgentTarget = Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            panic!("a callback that fails");
        });
        let (mut up, mut down) = spawn(Tap::new(BROWSER, callback));
        down.push(UPGRADED);
        up.push(&activate("T")); // panics on the observer thread, is contained
        assert!(
            eventually(Duration::from_secs(3), || up.off_reason()).is_some(),
            "the observer publishes the switch-off"
        );
        assert_eq!(up.off_reason(), Some("internal error"));
        up.push(&activate("T")); // the tap is off
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
