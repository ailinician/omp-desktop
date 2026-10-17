//! What a proxy connection hands the [`Tap`]: its CDP messages, observed on
//! a thread of their own.
//!
//! The connection's two pump threads each own the [`Feed`] of their
//! direction and call [`Feed::push`] on every message *before* forwarding
//! it. `push` only queues a message that fits the size cap (larger for one
//! event, see [`ATTACHED`]); a bigger one passes without being copied. Both
//! directions share one queue, so a command is queued before it can reach
//! Chromium and cause a reply, and the observer meets commands, replies and
//! the commands depending on them in causal order.
//!
//! Parsing JSON and the follow callback run on the observer thread only, and
//! the pumps never wait for it: a full queue switches the tap off for the
//! connection (both sides and the observer), never the forwarding.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, OnceLock};
use std::thread;

use super::tap::Tap;

/// Largest command the tap reads (a client message over it is skipped).
const MAX_COMMAND: usize = 1 << 20;
/// Largest reply or event the tap reads. Everything it needs is small; a
/// screenshot's reply is not, and is skipped without being copied.
const MAX_REPLY: usize = 64 << 10;
/// How a `Target.attachedToTarget` event starts. Chromium writes `method`
/// first; on a client's root session the broker rebuilds the message with
/// its keys sorted (dropping `sessionId`), and `method` < `params`, so the
/// event still starts so. It carries the page's URL and title in full, and
/// it is the only place an auto-attached session is named, so it is read up
/// to [`MAX_COMMAND`]: a page whose URL fits a command the tap reads (the
/// `Page.navigate` that opened it) is also followed.
const ATTACHED: &str = r#"{"method":"Target.attachedToTarget""#;
/// Messages queued for the observer of one connection, at most.
const QUEUE_MESSAGES: usize = 1024;
/// Bytes queued for the observer of one connection, at most: room for four
/// of the largest commands (or attach events) the tap reads, or sixty-four
/// of the largest replies. Skipped messages never count, so a burst of
/// screenshots costs nothing here; only an observer stuck for good fills it.
const QUEUE_BYTES: usize = 4 * MAX_COMMAND;

/// Which way a message went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Direction {
    /// Sent by the client (the agent).
    Client,
    /// Sent by the server (Chromium, through the broker).
    Server,
}

type Queued = (Direction, String);

/// What both feeds and the observer of a connection share.
#[derive(Default)]
struct State {
    /// Why the tap was switched off, once it was.
    off: OnceLock<&'static str>,
    /// Message bytes in the queue.
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

/// One direction's end of a connection's observer, owned by its pump thread.
pub(super) struct Feed {
    direction: Direction,
    queue: SyncSender<Queued>,
    state: Arc<State>,
    /// How long this feed sleeps in every `push` (the ordering test slows
    /// the client side of its own proxy).
    #[cfg(test)]
    delay: std::time::Duration,
}

/// Start the observer thread that owns `tap`; returns the client's and the
/// server's feed. The thread ends when both are gone. If it cannot start,
/// the feeds are born off: the connection goes on untapped.
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
    let feed = |direction, queue| Feed {
        direction,
        queue,
        state: Arc::clone(&state),
        #[cfg(test)]
        delay: std::time::Duration::ZERO,
    };
    let client = feed(Direction::Client, queue.clone());
    let server = feed(Direction::Server, queue);
    (client, server, messages, state)
}

impl Feed {
    /// Queue `message`, the next of this direction, when it fits the cap.
    /// Never blocks: the pump thread calls it before it forwards the
    /// message.
    pub(super) fn push(&self, message: &str) {
        if self.state.is_off() {
            return;
        }
        #[cfg(test)]
        thread::sleep(self.delay);
        let cap = match self.direction {
            Direction::Client => MAX_COMMAND,
            Direction::Server if message.starts_with(ATTACHED) => MAX_COMMAND,
            Direction::Server => MAX_REPLY,
        };
        if message.len() > cap {
            return;
        }
        if let Err(why) = self.enqueue(message) {
            self.state.off(why);
        }
    }

    fn enqueue(&self, message: &str) -> Result<(), &'static str> {
        let len = message.len();
        if self.state.queued.fetch_add(len, Ordering::AcqRel) + len > QUEUE_BYTES {
            return Err("observer queue full");
        }
        // The observer runs after the message is forwarded (and dropped):
        // it needs a copy of its own.
        match self.queue.try_send((self.direction, message.to_owned())) {
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

/// The observer thread: hands the messages to the tap in queue order.
fn observe(mut tap: Tap, messages: &Receiver<Queued>, state: &State) {
    for (direction, message) in messages {
        state.queued.fetch_sub(message.len(), Ordering::AcqRel);
        if state.is_off() {
            return;
        }
        if let Err(why) = tap.on_message(direction, message.as_bytes()) {
            return state.off(why);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::super::tests::Reported;
    use super::super::{tap::Tap, AgentTarget};
    use super::{channel, spawn, Direction, Queued, ATTACHED, MAX_REPLY, QUEUE_MESSAGES};
    use crate::browser::broker::fake::fake_browser;
    use crate::browser::broker::Recv;
    use crate::browser::lock;
    use crate::test_support::eventually;

    const BROWSER: &[u8] = b"GET /devtools/browser/x HTTP/1.1\r\nHost: h\r\n\r\n";

    /// What the feeds queued so far, in queue order.
    fn drained(queue: &Receiver<Queued>) -> Vec<Queued> {
        queue.try_iter().collect()
    }

    fn client(message: &str) -> Queued {
        (Direction::Client, message.to_owned())
    }

    fn server(message: &str) -> Queued {
        (Direction::Server, message.to_owned())
    }

    #[test]
    fn oversized_messages_are_never_queued() {
        let (up, down, queue, state) = channel();
        // A screenshot-sized reply, then one inside the cap.
        down.push(&"x".repeat(5 << 20));
        down.push("after");
        // A command over its cap, then one inside it.
        up.push(&"y".repeat(1_500_000));
        up.push("next");
        assert_eq!(drained(&queue), [server("after"), client("next")]);
        assert_eq!(
            state.queued.load(Ordering::Acquire),
            "after".len() + "next".len()
        );
        assert_eq!(down.off_reason(), None);
    }

    #[test]
    fn commands_and_replies_are_queued_in_the_order_they_were_pushed() {
        let (up, down, queue, _) = channel();
        up.push("c1");
        down.push("r1");
        down.push("e1");
        up.push("c2");
        down.push("r2");
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

    fn activate(target: &str) -> String {
        json!({ "id": 1, "method": "Target.activateTarget", "params": { "targetId": target } })
            .to_string()
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

    /// Waits until the gated callback was entered `n` times.
    fn entered(calls: &AtomicUsize, n: usize) -> bool {
        eventually(Duration::from_secs(3), || {
            (calls.load(Ordering::SeqCst) == n).then_some(())
        })
        .is_some()
    }

    #[test]
    fn a_large_reply_behind_a_stuck_observer_leaves_the_tap_on() {
        let gate = Arc::new(Mutex::new(()));
        let reported = Reported::default();
        let (tap, calls) = gated_tap(&gate, &reported);
        let held = lock(&gate);
        let (up, down) = spawn(tap);
        up.push(&activate("T1")); // the observer blocks in this callback
        assert!(entered(&calls, 1), "the observer reached the callback");
        // Screenshots far bigger than any queue, and one reply of the largest
        // size the tap reads, all while the observer is stuck.
        for _ in 0..3 {
            down.push(&"x".repeat(8 << 20));
        }
        down.push(&format!("\"{}\"", "y".repeat(MAX_REPLY - 2)));
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
    fn a_stuck_observer_disables_the_tap_without_blocking_the_pump() {
        let gate = Arc::new(Mutex::new(()));
        let reported = Reported::default();
        let (tap, calls) = gated_tap(&gate, &reported);
        let held = lock(&gate);
        let (up, _down) = spawn(tap);
        up.push(&activate("T")); // the observer blocks in this callback
        assert!(entered(&calls, 1));
        let start = Instant::now();
        let command = activate("T");
        for _ in 0..=QUEUE_MESSAGES {
            up.push(&command);
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
    fn queued_bytes_behind_a_stuck_observer_are_bounded() {
        let gate = Arc::new(Mutex::new(()));
        let reported = Reported::default();
        let (tap, calls) = gated_tap(&gate, &reported);
        let held = lock(&gate);
        let (up, _down) = spawn(tap);
        up.push(&activate("T")); // the observer blocks in this callback
        assert!(entered(&calls, 1));
        // Commands under the size cap, far fewer than QUEUE_MESSAGES.
        let command = "y".repeat(1_000_000);
        for _ in 0..4 {
            up.push(&command);
        }
        assert_eq!(up.off_reason(), None, "4 MB still fit");
        up.push(&command);
        assert_eq!(up.off_reason(), Some("observer queue full"));
        drop(held);
    }

    #[test]
    fn a_page_whose_attach_event_is_over_the_reply_cap_is_followed() {
        let reported = Reported::default();
        let (up, down) = spawn(Tap::new(BROWSER, reported.callback()));
        // Chromium's layout: `method` first, the URL in full.
        let attached = format!(
            r#"{{"method":"Target.attachedToTarget","params":{{"sessionId":"S1","targetInfo":{{"targetId":"T1","type":"page","title":"","url":"data:text/plain,{}"}},"waitingForDebugger":false}}}}"#,
            "x".repeat(4 * MAX_REPLY)
        );
        down.push(&attached);
        up.push(
            r#"{"id":2,"method":"Page.navigate","params":{"url":"about:blank"},"sessionId":"S1"}"#,
        );
        assert!(
            eventually(Duration::from_secs(3), || (!reported.all().is_empty())
                .then_some(()))
            .is_some(),
            "the session's page is known"
        );
        assert_eq!(reported.all(), ["T1"]);
        assert_eq!(up.off_reason(), None);
    }

    #[test]
    fn an_attach_event_on_a_root_session_still_starts_with_the_prefix() {
        // `params` ahead of `method`: the broker's rebuild puts them in order.
        let event = json!({
            "params": {
                "sessionId": "S1",
                "targetInfo": { "targetId": "T1", "type": "page", "url": "about:blank" },
                "waitingForDebugger": false,
            },
            "method": "Target.attachedToTarget",
        });
        // Sent on the client's root session (the fake adds its `sessionId`).
        let broker = fake_browser(move |_| Some(vec![event.clone()]));
        let client = broker.connect_browser().unwrap();
        client
            .send(r#"{"id":1,"method":"Target.setAutoAttach","params":{}}"#)
            .unwrap();
        let Recv::Message(text) = client.recv(Duration::from_secs(5)) else {
            panic!("no event");
        };
        assert!(text.starts_with(ATTACHED), "{text}");
        assert!(!text.contains(r#""sessionId":"root"#), "{text}");
    }
}
