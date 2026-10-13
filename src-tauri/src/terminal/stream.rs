//! One terminal's output state, shared by its pump, its waiter and its
//! [`super::pty::Pty`]: the output ring, the flow-control debt, the closed
//! flag and the shell's exit.
//!
//! **Flow control.** The pump counts the bytes emitted but not yet acked by
//! the webview. At [`HIGH_WATER`] it stops emitting *and* stops draining its
//! channel, so the reader blocks on the full queue and the pty throttles the
//! shell; it resumes once acks bring the debt below [`LOW_WATER`], or a
//! replay resets it. A stalled pump waits on a condvar that `ack`, `replay`
//! and `close` notify.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};

use super::ring::{OutputRing, Replay};

/// Unacked bytes at which the pump stalls.
pub const HIGH_WATER: u64 = 512 * 1024;
/// A stalled pump resumes once the unacked bytes are below this.
pub const LOW_WATER: u64 = 128 * 1024;

/// Whether the shell still runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShellState {
    Running,
    /// Exited with this code; `None` = killed by a signal.
    Exited(Option<i32>),
}

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
pub enum Credit {
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

    pub fn shell_state(&self) -> ShellState {
        self.exit
            .get()
            .map_or(ShellState::Running, |&code| ShellState::Exited(code))
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

    pub fn is_closed(&self) -> bool {
        self.flow().closed
    }

    /// Block while the debt is at [`HIGH_WATER`] until it drops below
    /// [`LOW_WATER`] (or is forgiven), or the terminal is closed.
    pub fn credit(&self) -> Credit {
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
    pub fn record(&self, chunk: Vec<u8>) -> Option<(u64, Arc<Vec<u8>>)> {
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
