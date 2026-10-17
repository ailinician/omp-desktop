//! A queue of CDP messages, bounded in bytes and never blocking whoever
//! fills it: a broker client's messages from Chromium (a client that stops
//! reading is cut instead of growing the app's memory), and the broker's
//! messages waiting for the pipe to Chromium.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use super::super::lock;

/// Bytes that may wait behind the next message out. That message does not
/// count, so one larger than this still goes (a full-page screenshot reply,
/// say).
const MAX_QUEUED: usize = 64 << 20;

/// What [`Outbox::recv`] gives back.
#[derive(Debug, PartialEq, Eq)]
pub enum Recv {
    Message(String),
    /// Nothing arrived within the timeout.
    Timeout,
    /// The client or the browser connection ended, and everything queued
    /// before that has been received.
    Closed,
}

/// Why [`Outbox::offer`] refused a message.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Closed,
    /// It would overflow the queue, which stays open.
    Full,
}

#[derive(Default)]
struct State {
    items: VecDeque<String>,
    bytes: usize,
    closed: bool,
}

impl State {
    /// Whether `len` more bytes stay within `limit`, not counting the
    /// message at the head of the queue.
    fn fits(&self, len: usize, limit: usize) -> bool {
        self.items
            .front()
            .is_none_or(|front| self.bytes - front.len() + len <= limit)
    }

    fn push(&mut self, text: String) {
        self.bytes += text.len();
        self.items.push_back(text);
    }
}

pub struct Outbox {
    state: Mutex<State>,
    ready: Condvar,
    limit: usize,
}

impl Default for Outbox {
    fn default() -> Self {
        Self::with_limit(MAX_QUEUED)
    }
}

impl Outbox {
    fn with_limit(limit: usize) -> Self {
        Self {
            state: Mutex::default(),
            ready: Condvar::new(),
            limit,
        }
    }

    /// Queue `text`. `false` when the outbox is closed, or was just closed
    /// (its backlog dropped) because `text` would overflow it.
    pub fn push(&self, text: String) -> bool {
        let mut state = lock(&self.state);
        if state.closed {
            return false;
        }
        if !state.fits(text.len(), self.limit) {
            state.closed = true;
            state.items.clear();
            state.bytes = 0;
            drop(state);
            self.ready.notify_all();
            return false;
        }
        state.push(text);
        drop(state);
        self.ready.notify_one();
        true
    }

    /// Queue `text` unless the outbox is closed or `text` would overflow it;
    /// a refusal leaves the outbox as it was.
    pub fn offer(&self, text: String) -> Result<(), Refused> {
        let mut state = lock(&self.state);
        if state.closed {
            return Err(Refused::Closed);
        }
        if !state.fits(text.len(), self.limit) {
            return Err(Refused::Full);
        }
        state.push(text);
        drop(state);
        self.ready.notify_one();
        Ok(())
    }

    /// No more messages; what is queued can still be received with
    /// [`recv`](Self::recv).
    pub fn close(&self) {
        lock(&self.state).closed = true;
        self.ready.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }

    /// The next message, waiting up to `timeout` for one.
    pub fn recv(&self, timeout: Duration) -> Recv {
        let (mut state, _) = self
            .ready
            .wait_timeout_while(lock(&self.state), timeout, |s| {
                s.items.is_empty() && !s.closed
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let next = state.items.pop_front();
        if let Some(text) = &next {
            state.bytes -= text.len();
        }
        let closed = state.closed;
        drop(state);
        match next {
            Some(text) => Recv::Message(text),
            None if closed => Recv::Closed,
            None => Recv::Timeout,
        }
    }

    /// The next message, waiting for as long as it takes; `None` once the
    /// outbox is closed, even with messages left (they are not wanted any
    /// more).
    pub fn next_until_closed(&self) -> Option<String> {
        let mut state = self
            .ready
            .wait_while(lock(&self.state), |s| s.items.is_empty() && !s.closed)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let text = if state.closed {
            None
        } else {
            state.items.pop_front()
        };
        if let Some(text) = &text {
            state.bytes -= text.len();
        }
        drop(state);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: Duration = Duration::ZERO;

    #[test]
    fn messages_come_out_in_order_and_queued_ones_survive_close() {
        let outbox = Outbox::default();
        assert!(outbox.push("a".into()));
        assert!(outbox.push("b".into()));
        outbox.close();
        assert!(!outbox.push("c".into()), "closed: refused");
        assert_eq!(outbox.recv(NOW), Recv::Message("a".into()));
        assert_eq!(outbox.recv(NOW), Recv::Message("b".into()));
        assert_eq!(outbox.recv(NOW), Recv::Closed);
    }

    #[test]
    fn an_empty_open_outbox_times_out() {
        assert_eq!(
            Outbox::default().recv(Duration::from_millis(10)),
            Recv::Timeout
        );
    }

    #[test]
    fn the_message_at_the_head_does_not_count_toward_overflow() {
        let outbox = Outbox::with_limit(10);
        assert!(outbox.push("h".repeat(11)), "alone: accepted");
        assert!(outbox.push("x".repeat(10)), "the head is not counted");
        assert!(
            !outbox.push("y".into()),
            "a byte over the cap behind the head cuts the client"
        );
        assert!(outbox.is_closed());
        assert_eq!(outbox.recv(NOW), Recv::Closed, "the backlog is dropped");
    }

    #[test]
    fn the_cap_moves_with_the_head() {
        let outbox = Outbox::with_limit(10);
        assert!(outbox.push("a".repeat(6)));
        assert!(outbox.push("b".repeat(6)));
        assert!(matches!(outbox.recv(NOW), Recv::Message(m) if m == "a".repeat(6)));
        // "b" is the head now: a full cap fits behind it, a byte more does not.
        assert!(outbox.push("c".repeat(10)));
        assert!(!outbox.push("d".into()));
    }

    #[test]
    fn offer_refuses_without_closing() {
        let outbox = Outbox::with_limit(10);
        assert_eq!(outbox.offer("h".repeat(20)), Ok(()));
        assert_eq!(outbox.offer("x".repeat(10)), Ok(()));
        assert_eq!(outbox.offer("y".into()), Err(Refused::Full));
        assert!(!outbox.is_closed());
        assert_eq!(outbox.next_until_closed(), Some("h".repeat(20)));
        // "x" is the head now and no longer counts.
        assert_eq!(outbox.offer("y".into()), Ok(()));
        assert_eq!(outbox.offer("z".repeat(10)), Err(Refused::Full));
        assert_eq!(outbox.recv(NOW), Recv::Message("x".repeat(10)));
        outbox.close();
        assert_eq!(outbox.offer("z".into()), Err(Refused::Closed));
        assert_eq!(
            outbox.next_until_closed(),
            None,
            "closed: the rest is dropped"
        );
    }

    #[test]
    fn close_wakes_a_waiting_reader() {
        let outbox = std::sync::Arc::new(Outbox::default());
        let waiting = std::sync::Arc::clone(&outbox);
        let reader = std::thread::spawn(move || waiting.recv(Duration::from_secs(30)));
        let taker = std::sync::Arc::clone(&outbox);
        let writer = std::thread::spawn(move || taker.next_until_closed());
        std::thread::sleep(Duration::from_millis(50));
        outbox.close();
        assert_eq!(reader.join().unwrap(), Recv::Closed);
        assert_eq!(writer.join().unwrap(), None);
    }
}
