//! The recent output of one terminal, for a webview that missed some of it
//! (created late, reloaded, or saw a gap in the event `seq`s).
//!
//! Every data event the pump emits is appended here first, under the seq it
//! goes out with. The ring keeps whole events, newest last, within a byte
//! budget: the oldest are evicted as a unit, so a replay never starts inside
//! an escape sequence that one event split.

use std::collections::VecDeque;
use std::sync::Arc;

/// Byte budget of one terminal's ring.
pub const RING_BYTES: usize = 1024 * 1024;

/// What [`OutputRing::replay`] returns.
#[derive(Debug, PartialEq, Eq)]
pub struct Replay {
    /// The held output after the requested seq, in order.
    pub bytes: Vec<u8>,
    /// The seq of the newest event emitted so far (0 before the first).
    pub head_seq: u64,
    /// Some output after the requested seq was already evicted: `bytes`
    /// does not continue where the caller left off.
    pub dropped: bool,
}

/// Emitted chunks by seq (1, 2, … per terminal), within a byte budget.
pub struct OutputRing {
    chunks: VecDeque<(u64, Arc<Vec<u8>>)>,
    /// Sum of the held chunks' lengths.
    held: usize,
    budget: usize,
    /// The seq of the newest chunk pushed (0 before the first).
    head_seq: u64,
    /// The seq of the newest chunk evicted (0 before the first eviction).
    evicted_seq: u64,
}

impl OutputRing {
    pub const fn new() -> Self {
        Self::with_budget(RING_BYTES)
    }

    pub const fn with_budget(budget: usize) -> Self {
        Self {
            chunks: VecDeque::new(),
            held: 0,
            budget,
            head_seq: 0,
            evicted_seq: 0,
        }
    }

    /// Append `chunk` under the next seq and return that seq, evicting the
    /// oldest chunks beyond the budget. A chunk larger than the whole budget
    /// is still kept (alone): the newest output is never the one lost.
    pub fn push(&mut self, chunk: Arc<Vec<u8>>) -> u64 {
        self.head_seq += 1;
        self.held += chunk.len();
        self.chunks.push_back((self.head_seq, chunk));
        while self.held > self.budget && self.chunks.len() > 1 {
            if let Some((seq, old)) = self.chunks.pop_front() {
                self.held -= old.len();
                self.evicted_seq = seq;
            }
        }
        self.head_seq
    }

    /// The held output of every chunk with a seq after `after_seq`.
    pub fn replay(&self, after_seq: u64) -> Replay {
        let wanted = || self.chunks.iter().filter(|(seq, _)| *seq > after_seq);
        let mut bytes = Vec::with_capacity(wanted().map(|(_, chunk)| chunk.len()).sum());
        for (_, chunk) in wanted() {
            bytes.extend_from_slice(chunk);
        }
        Replay {
            bytes,
            head_seq: self.head_seq,
            dropped: self.evicted_seq > after_seq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn push(ring: &mut OutputRing, bytes: &[u8]) -> u64 {
        ring.push(Arc::new(bytes.to_vec()))
    }

    #[test]
    fn an_empty_ring_replays_nothing_at_seq_zero() {
        let ring = OutputRing::new();
        assert_eq!(
            ring.replay(0),
            Replay {
                bytes: Vec::new(),
                head_seq: 0,
                dropped: false
            }
        );
    }

    #[test]
    fn seqs_count_up_from_one_and_replay_resumes_after_the_given_one() {
        let mut ring = OutputRing::new();
        assert_eq!(push(&mut ring, b"ab"), 1);
        assert_eq!(push(&mut ring, b"cd"), 2);
        assert_eq!(push(&mut ring, b"ef"), 3);
        let all = ring.replay(0);
        assert_eq!(
            (all.bytes.as_slice(), all.head_seq, all.dropped),
            (&b"abcdef"[..], 3, false)
        );
        let tail = ring.replay(2);
        assert_eq!(
            (tail.bytes.as_slice(), tail.head_seq, tail.dropped),
            (&b"ef"[..], 3, false)
        );
        let none = ring.replay(3);
        assert_eq!(
            (none.bytes.len(), none.head_seq, none.dropped),
            (0, 3, false)
        );
    }

    #[test]
    fn whole_chunks_are_evicted_oldest_first_and_reported_as_dropped() {
        let mut ring = OutputRing::with_budget(10);
        push(&mut ring, b"1111"); // seq 1
        push(&mut ring, b"2222"); // seq 2
        push(&mut ring, b"3333"); // seq 3: 12 > 10, seq 1 goes
        let all = ring.replay(0);
        assert_eq!(all.bytes, b"22223333", "evicted mid-chunk or kept too much");
        assert!(all.dropped, "seq 1 was evicted");
        assert_eq!(all.head_seq, 3);
        // Nothing after seq 1 was lost.
        let rest = ring.replay(1);
        assert_eq!(
            (rest.bytes.as_slice(), rest.dropped),
            (&b"22223333"[..], false)
        );
    }

    #[test]
    fn an_oversized_chunk_is_kept_alone() {
        let mut ring = OutputRing::with_budget(4);
        push(&mut ring, b"ab");
        push(&mut ring, b"0123456789");
        let all = ring.replay(0);
        assert_eq!(
            (all.bytes.as_slice(), all.head_seq, all.dropped),
            (&b"0123456789"[..], 2, true)
        );
    }

    #[test]
    fn the_budget_holds_over_many_events() {
        let mut ring = OutputRing::new();
        for _ in 0..40 {
            ring.push(Arc::new(vec![7; 64 * 1024]));
        }
        let all = ring.replay(0);
        assert_eq!(all.bytes.len(), RING_BYTES);
        assert_eq!(all.head_seq, 40);
        assert!(all.dropped);
        assert!(!ring.replay(24).dropped, "seq 25..=40 are all held");
        assert!(ring.replay(23).dropped, "seq 24 was evicted");
    }
}
