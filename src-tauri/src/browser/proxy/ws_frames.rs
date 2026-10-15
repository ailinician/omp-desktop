//! Incremental decoder of one direction of a WebSocket (RFC 6455 framing):
//! bytes in, complete text messages out. Run by the proxy's pipe threads
//! (see [`feed`](super::feed)) on every chunk before it is forwarded.
//!
//! Frames are decoded as they stream by. A message over the size cap is
//! skipped from the length in its frame header: none of it is copied (only
//! the first few bytes of one that may be read to a larger cap, see
//! [`Messages::reading_up_to`]), and decoding stays in sync behind it.
//! Control frames (which may interleave with the fragments of a message) and
//! binary messages are dropped. A stream the decoder cannot follow - broken
//! framing, or an RSV bit that announces a negotiated extension - is
//! [`Lost`].

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
/// Control frames have the high bit of the opcode set.
const OP_CONTROL: u8 = 0x8;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;
/// Largest payload of a control frame.
const MAX_CONTROL: u64 = 125;

/// The stream cannot be followed any more.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Lost;

/// A frame whose header is read; `remaining` payload bytes are to come.
struct Frame {
    opcode: u8,
    fin: bool,
    mask: Option<[u8; 4]>,
    remaining: u64,
    /// Payload bytes of this frame decoded so far, modulo 4 (the mask phase).
    phase: usize,
}

pub(super) struct Messages {
    /// Whether the peer sending this direction must mask (clients do,
    /// servers do not).
    masked: bool,
    max: usize,
    /// The header bytes read so far of the next frame.
    head: Vec<u8>,
    frame: Option<Frame>,
    /// The data message being assembled.
    message: Vec<u8>,
    /// A data message is open: its final fragment has not come yet.
    open: bool,
    /// The open message is not wanted (binary, or over the cap).
    skipping: bool,
    /// Messages starting with these bytes are read up to this larger cap.
    larger: Option<(&'static [u8], usize)>,
}

impl Messages {
    /// A decoder for the client-to-server direction (`from_client`) or the
    /// other; messages over `max` bytes are skipped.
    pub fn new(from_client: bool, max: usize) -> Self {
        Self {
            masked: from_client,
            max,
            head: Vec::with_capacity(14),
            frame: None,
            message: Vec::new(),
            open: false,
            skipping: false,
            larger: None,
        }
    }

    /// Also reads the messages that start with `prefix` up to `max` bytes.
    /// Of any other message over the cap, at most `prefix.len()` bytes are
    /// copied to tell it apart.
    pub const fn reading_up_to(mut self, prefix: &'static [u8], max: usize) -> Self {
        self.larger = Some((prefix, max));
        self
    }

    /// Decode `bytes`, appending each text message that completes to `out`.
    pub fn feed(&mut self, mut bytes: &[u8], out: &mut Vec<Vec<u8>>) -> Result<(), Lost> {
        while !bytes.is_empty() {
            let Some(mut frame) = self.frame.take() else {
                self.head.push(bytes[0]);
                bytes = &bytes[1..];
                if let Some(frame) = parse_header(&self.head, self.masked)? {
                    self.head.clear();
                    self.start(&frame)?;
                    self.continue_or_end(frame, out);
                }
                continue;
            };
            let take = usize::try_from(frame.remaining).map_or(bytes.len(), |r| r.min(bytes.len()));
            let (chunk, rest) = bytes.split_at(take);
            bytes = rest;
            self.payload(&mut frame, chunk);
            frame.remaining -= chunk.len() as u64;
            self.continue_or_end(frame, out);
        }
        Ok(())
    }

    /// Keep `frame` for its payload, or finish it when none is left.
    fn continue_or_end(&mut self, frame: Frame, out: &mut Vec<Vec<u8>>) {
        if frame.remaining > 0 {
            self.frame = Some(frame);
        } else if frame.opcode < OP_CONTROL && frame.fin {
            self.open = false;
            let message = std::mem::take(&mut self.message);
            if !self.skipping {
                out.push(message);
            }
        }
    }

    /// A frame header is complete: check it fits the message in progress,
    /// and skip the message right away when its length already says it
    /// cannot.
    fn start(&mut self, frame: &Frame) -> Result<(), Lost> {
        match frame.opcode {
            OP_CONT if !self.open => Err(Lost),
            OP_TEXT | OP_BINARY if self.open => Err(Lost),
            OP_TEXT | OP_BINARY => {
                self.open = true;
                self.skipping = frame.opcode == OP_BINARY;
                self.message.clear();
                self.skip_if_over(frame.remaining);
                Ok(())
            }
            OP_CONT => {
                self.skip_if_over(frame.remaining);
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Skips the open message if `more` bytes would take it over its cap.
    fn skip_if_over(&mut self, more: u64) {
        let len = u64::try_from(self.message.len()).unwrap_or(u64::MAX);
        let limit = u64::try_from(self.limit()).unwrap_or(u64::MAX);
        if !self.skipping && len.saturating_add(more) > limit {
            self.skip();
        }
    }

    fn skip(&mut self) {
        self.skipping = true;
        self.message = Vec::new(); // give the memory back
    }

    /// The cap of the open message: the larger one while what came of it so
    /// far is, or may still become, the prefix read to it.
    fn limit(&self) -> usize {
        match self.larger {
            Some((prefix, max))
                if self.message.starts_with(prefix) || prefix.starts_with(&self.message) =>
            {
                max.max(self.max)
            }
            _ => self.max,
        }
    }

    /// Bytes of the open message still to come before its cap is known.
    fn undecided(&self) -> usize {
        match self.larger {
            Some((prefix, _)) if prefix.starts_with(&self.message) => {
                prefix.len() - self.message.len()
            }
            _ => 0,
        }
    }

    /// Unmask `chunk` of the data frame into the message, unless it is
    /// skipped. A frame header announcing too much has skipped it already;
    /// a message whose first bytes turn out not to be the prefix read to
    /// the larger cap is skipped as soon as they are in.
    fn payload(&mut self, frame: &mut Frame, chunk: &[u8]) {
        if frame.opcode >= OP_CONTROL || self.skipping {
            return;
        }
        let (probe, rest) = chunk.split_at(self.undecided().min(chunk.len()));
        self.append(frame, probe);
        // `remaining` still counts all of `chunk`.
        self.skip_if_over(frame.remaining - probe.len() as u64);
        if !self.skipping {
            self.append(frame, rest);
        }
    }

    fn append(&mut self, frame: &mut Frame, bytes: &[u8]) {
        let from = self.message.len();
        self.message.extend_from_slice(bytes);
        if let Some(key) = frame.mask {
            for (i, byte) in self.message[from..].iter_mut().enumerate() {
                *byte ^= key[(frame.phase + i) % 4];
            }
        }
        frame.phase = (frame.phase + bytes.len()) % 4;
    }
}

/// The frame whose header is `head`; `None` while `head` is incomplete.
fn parse_header(head: &[u8], masked: bool) -> Result<Option<Frame>, Lost> {
    let [first, second, rest @ ..] = head else {
        return Ok(None);
    };
    if first & 0x70 != 0 {
        return Err(Lost); // an RSV bit: a negotiated extension
    }
    let (fin, opcode) = (first & 0x80 != 0, first & 0x0F);
    if !matches!(
        opcode,
        OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) || (second & 0x80 != 0) != masked
    {
        return Err(Lost);
    }
    let (length, rest) = match second & 0x7F {
        126 => match rest.split_first_chunk::<2>() {
            Some((len, rest)) => (u64::from(u16::from_be_bytes(*len)), rest),
            None => return Ok(None),
        },
        127 => match rest.split_first_chunk::<8>() {
            Some((len, rest)) => {
                let length = u64::from_be_bytes(*len);
                if length >> 63 != 0 {
                    return Err(Lost);
                }
                (length, rest)
            }
            None => return Ok(None),
        },
        short => (u64::from(short), rest),
    };
    if opcode >= OP_CONTROL && (length > MAX_CONTROL || !fin) {
        return Err(Lost);
    }
    let mask = if masked {
        match rest.first_chunk::<4>() {
            Some(key) => Some(*key),
            None => return Ok(None),
        }
    } else {
        None
    };
    Ok(Some(Frame {
        opcode,
        fin,
        mask,
        remaining: length,
        phase: 0,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{frame, masked_text, text};
    use super::{Lost, Messages};

    fn feed_all(decoder: &mut Messages, chunks: &[&[u8]]) -> Result<Vec<String>, Lost> {
        let mut out = Vec::new();
        for chunk in chunks {
            decoder.feed(chunk, &mut out)?;
        }
        Ok(out
            .into_iter()
            .map(|m| String::from_utf8(m).unwrap())
            .collect())
    }

    fn client() -> Messages {
        Messages::new(true, 1 << 20)
    }

    fn server(max: usize) -> Messages {
        Messages::new(false, max)
    }

    #[test]
    fn masked_and_unmasked_frames_of_every_length_encoding_decode() {
        for len in [0, 1, 125, 126, 300, 65_535, 65_536, 70_000] {
            let payload = "x".repeat(len);
            assert_eq!(
                feed_all(&mut client(), &[&masked_text(&payload)]),
                Ok(vec![payload.clone()]),
                "client, {len}"
            );
            assert_eq!(
                feed_all(&mut server(1 << 20), &[&text(&payload)]),
                Ok(vec![payload]),
                "server, {len}"
            );
        }
    }

    #[test]
    fn any_chunking_gives_the_same_messages() {
        let mut wire = masked_text("one");
        wire.extend(frame(0x9, true, true, b"ping"));
        wire.extend(frame(0x1, false, true, b"tw"));
        wire.extend(frame(0xA, true, true, b"")); // a control frame between fragments
        wire.extend(frame(0x0, false, true, b"o-"));
        wire.extend(frame(0x0, true, true, b"fragments"));
        wire.extend(masked_text(&"y".repeat(300)));
        let expected = vec![
            "one".to_owned(),
            "two-fragments".to_owned(),
            "y".repeat(300),
        ];
        for step in [1, 2, 3, 5, 7, wire.len()] {
            let mut decoder = client();
            let chunks: Vec<&[u8]> = wire.chunks(step).collect();
            assert_eq!(
                feed_all(&mut decoder, &chunks),
                Ok(expected.clone()),
                "{step}"
            );
        }
    }

    #[test]
    fn binary_messages_and_close_frames_are_dropped() {
        let mut wire = frame(0x2, true, false, b"\x00\x01");
        wire.extend(frame(0x2, false, false, b"a"));
        wire.extend(frame(0x0, true, false, b"b"));
        wire.extend(text("kept"));
        wire.extend(frame(0x8, true, false, b"\x03\xe8"));
        assert_eq!(
            feed_all(&mut server(1 << 20), &[&wire]),
            Ok(vec!["kept".to_owned()])
        );
    }

    #[test]
    fn a_message_over_the_cap_is_skipped_and_decoding_stays_in_sync() {
        let mut wire = frame(0x1, false, false, &[b'a'; 40]);
        wire.extend(frame(0x9, true, false, b"ping"));
        wire.extend(frame(0x0, false, false, &[b'b'; 40]));
        wire.extend(frame(0x0, true, false, &[b'c'; 40]));
        wire.extend(text("next"));
        wire.extend(text(&"z".repeat(101))); // one frame over the cap
        wire.extend(text("last"));
        assert_eq!(
            feed_all(&mut server(100), &[&wire]),
            Ok(vec!["next".to_owned(), "last".to_owned()])
        );
    }

    #[test]
    fn a_message_over_the_cap_is_skipped_from_its_header_without_copying() {
        // One frame announcing more than the cap; only its header and fewer
        // payload bytes than the cap have arrived.
        let wire = text(&"z".repeat(5000));
        let mut decoder = server(100);
        let mut out = Vec::new();
        decoder.feed(&wire[..4 + 50], &mut out).unwrap();
        assert_eq!(decoder.message.capacity(), 0, "nothing of it copied");
        decoder.feed(&wire[4 + 50..], &mut out).unwrap();
        decoder.feed(&text("next"), &mut out).unwrap();
        assert_eq!(out, [b"next".to_vec()]);

        // A fragment announcing more than is left of the cap lets go of the
        // fragments before it.
        let mut wire = frame(0x1, false, false, &[b'a'; 40]);
        wire.extend(frame(0x0, true, false, &[b'b'; 100]));
        let mut decoder = server(100);
        decoder.feed(&wire[..42 + 2 + 10], &mut out).unwrap();
        assert_eq!(decoder.message.capacity(), 0, "nothing kept past the cap");

        // A client's masked frame: nothing copied or unmasked either.
        let wire = masked_text(&"z".repeat(5000));
        let mut decoder = Messages::new(true, 100);
        decoder.feed(&wire[..4 + 4 + 50], &mut out).unwrap();
        assert_eq!(decoder.message.capacity(), 0);
    }

    #[test]
    fn a_message_with_the_prefix_is_read_to_the_larger_cap() {
        let long = format!("PRE{}", "x".repeat(500));
        let other = "y".repeat(500);
        let mut wire = text(&long);
        wire.extend(text(&other));
        wire.extend(text(&format!("PRE{}", "x".repeat(1000)))); // over the larger cap
        wire.extend(text("PR")); // short, so under the cap either way
        wire.extend(frame(0x1, false, false, b"PRE"));
        wire.extend(frame(0x0, true, false, &[b'f'; 300]));
        wire.extend(text("small"));
        let expected = vec![
            long,
            "PR".to_owned(),
            format!("PRE{}", "f".repeat(300)),
            "small".to_owned(),
        ];
        for step in [1, 2, 7, wire.len()] {
            let mut decoder = server(100).reading_up_to(b"PRE", 1000);
            let chunks: Vec<&[u8]> = wire.chunks(step).collect();
            assert_eq!(
                feed_all(&mut decoder, &chunks),
                Ok(expected.clone()),
                "{step}"
            );
        }
        // Another message over the cap: let go once its first bytes differ.
        let wire = text(&other);
        let mut decoder = server(100).reading_up_to(b"PRE", 1000);
        decoder.feed(&wire[..4 + 50], &mut Vec::new()).unwrap();
        assert_eq!(decoder.message.capacity(), 0, "only the probe was copied");
    }

    #[test]
    fn a_stream_that_breaks_the_framing_is_lost() {
        let cases: [(&str, bool, Vec<u8>); 8] = [
            ("a client frame without a mask", true, text("x")),
            ("a server frame with a mask", false, masked_text("x")),
            ("an RSV bit", false, vec![0xC1, 0x01, b'x']),
            ("an unknown opcode", false, vec![0x83, 0x00]),
            (
                "a continuation with nothing to continue",
                false,
                frame(0x0, true, false, b"x"),
            ),
            (
                "a fragmented control frame",
                false,
                frame(0x9, false, false, b""),
            ),
            (
                "a control frame over 125 bytes",
                false,
                frame(0x9, true, false, &[0; 126]),
            ),
            (
                "a length with its top bit set",
                false,
                vec![0x81, 127, 0x80, 0, 0, 0, 0, 0, 0, 0],
            ),
        ];
        for (what, from_client, wire) in cases {
            let mut decoder = Messages::new(from_client, 1 << 20);
            assert_eq!(feed_all(&mut decoder, &[&wire]), Err(Lost), "{what}");
        }
        let mut wire = frame(0x1, false, false, b"a");
        wire.extend(text("a new message while one is open"));
        assert_eq!(feed_all(&mut server(100), &[&wire]), Err(Lost));
    }
}
