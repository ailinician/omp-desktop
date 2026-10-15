//! Payloads and sink of the browser panel's messages (one ordered channel
//! per open panel), and the frame's pixel size read from its JPEG header.

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tauri::ipc::{Channel, InvokeResponseBody};

/// One screencast frame. `b64` is Chromium's JPEG, as Chromium sent it
/// (decoded only when it goes out). `w`/`h` are image pixels; `css_w`/`css_h`
/// the page's CSS viewport the frame shows (clicks map through those). `seq`
/// numbers the frames a view emits; the webview acks it
/// (`browser_frame_ack`) once painted or dropped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub b64: String,
    pub w: u32,
    pub h: u32,
    pub css_w: u32,
    pub css_h: u32,
    pub seq: u64,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub target_id: String,
    pub title: String,
    pub url: String,
    pub active: bool,
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Starting,
    Running,
    Stopped,
    Error,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
// The panel's `state` message: independent flags the webview reads.
#[allow(clippy::struct_excessive_bools)]
pub struct ViewState {
    pub status: Status,
    pub error: Option<String>,
    pub url: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    /// Annotate mode is on (Rust's truth; the toolbar shows it).
    pub annotate: bool,
}

impl ViewState {
    pub const fn new(status: Status) -> Self {
        Self {
            status,
            error: None,
            url: String::new(),
            loading: false,
            can_back: false,
            can_forward: false,
            annotate: false,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// An element or area the user picked in the page (annotate mode).
/// `kind` is `element` or `area`; `selector`, `tag`, `html` and `text` are
/// `None` for an area.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Pick {
    pub kind: String,
    pub selector: Option<String>,
    pub tag: Option<String>,
    pub rect: Rect,
    /// Document CSS pixels at pick time; `rect` remains viewport-relative.
    pub doc_rect: Option<Rect>,
    pub html: Option<String>,
    pub text: Option<String>,
    pub url: String,
}

/// Where the view reports to; `session` is the omp tab's session id.
pub trait BrowserEmit: Send + Sync {
    fn frame(&self, sid: &str, f: &Frame);
    fn tabs(&self, sid: &str, tabs: &[TabInfo]);
    fn state(&self, sid: &str, s: &ViewState);
    fn pick(&self, sid: &str, p: &Pick);
}

/// A JSON message of the panel's channel: `{ type, value }`.
#[derive(Serialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
enum Message<'a> {
    State(&'a ViewState),
    Tabs(&'a [TabInfo]),
    Pick(&'a Pick),
}

/// Sends the panel's messages over the channel the webview handed to
/// `browser_open_panel`. One channel keeps them in order (a page's `tabs`
/// before its frames) and carries a frame as raw bytes: no base64 or JSON
/// for the webview to parse. The session is the channel's own.
pub struct TauriBrowserEmit(pub Channel<InvokeResponseBody>);

impl TauriBrowserEmit {
    fn json(&self, message: &Message) {
        if let Ok(json) = serde_json::to_string(message) {
            let _ = self.0.send(InvokeResponseBody::Json(json));
        }
    }
}

impl BrowserEmit for TauriBrowserEmit {
    fn frame(&self, _: &str, f: &Frame) {
        if let Some(bytes) = f.wire() {
            let _ = self.0.send(InvokeResponseBody::Raw(bytes));
        }
    }

    fn tabs(&self, _: &str, tabs: &[TabInfo]) {
        self.json(&Message::Tabs(tabs));
    }

    fn state(&self, _: &str, s: &ViewState) {
        self.json(&Message::State(s));
    }

    fn pick(&self, _: &str, p: &Pick) {
        self.json(&Message::Pick(p));
    }
}

impl Frame {
    /// A frame of Chromium's base64 JPEG, untouched. The image size is read
    /// from the JPEG header (the screencast metadata only carries the CSS
    /// size); without a readable header it falls back to the CSS size. `seq`
    /// is given when the frame is emitted.
    pub fn new(b64: String, css_w: u32, css_h: u32) -> Self {
        let (w, h) = jpeg_size(&b64).unwrap_or((css_w, css_h));
        Self {
            b64,
            w,
            h,
            css_w,
            css_h,
            seq: 0,
        }
    }

    /// The frame as the panel's channel carries it: `seq` (u64), `css_w`
    /// and `css_h` (u32), little-endian, then the JPEG. `None` when the
    /// base64 does not decode.
    pub fn wire(&self) -> Option<Vec<u8>> {
        let mut bytes = Vec::with_capacity(FRAME_HEADER + self.b64.len() / 4 * 3);
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.extend_from_slice(&self.css_w.to_le_bytes());
        bytes.extend_from_slice(&self.css_h.to_le_bytes());
        STANDARD.decode_vec(&self.b64, &mut bytes).ok()?;
        Some(bytes)
    }

    /// Whether the image has the proportions of the CSS size the metadata
    /// claims, give or take the rounding of its pixel size. Chromium's
    /// first frame of a screencast can be the whole window surface labelled
    /// with the emulated viewport's size; clicks mapped through such a
    /// label would land beside the target.
    pub fn has_css_proportions(&self) -> bool {
        let across = u64::from(self.w) * u64::from(self.css_h);
        let down = u64::from(self.h) * u64::from(self.css_w);
        // Each image side is rounded by up to half a pixel.
        across.abs_diff(down) <= u64::from(self.css_w.max(self.css_h))
    }
}

/// Bytes before the JPEG in [`Frame::wire`].
const FRAME_HEADER: usize = 16;

/// Base64 characters decoded for [`jpeg_size`]; the start-of-frame segment
/// sits well before.
const PREFIX: usize = 4096;

/// The bytes of the first `PREFIX` base64 characters, cut to whole groups
/// (`PREFIX` is a multiple of 4; a shorter string keeps its padding).
fn decode_prefix(b64: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(&b64.as_bytes()[..b64.len().min(PREFIX) & !3])
        .ok()
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from(*bytes.get(at)?) << 8 | u32::from(*bytes.get(at + 1)?))
}

/// `(width, height)` in pixels of the JPEG `b64` encodes.
fn jpeg_size(b64: &str) -> Option<(u32, u32)> {
    let bytes = decode_prefix(b64)?;
    if bytes.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    loop {
        if *bytes.get(at)? != 0xFF {
            return None;
        }
        let marker = *bytes.get(at + 1)?;
        // Start of frame: SOF0..SOF15 except DHT (C4), JPG (C8) and DAC (CC).
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            return Some((be16(&bytes, at + 7)?, be16(&bytes, at + 5)?));
        }
        at += 2 + usize::try_from(be16(&bytes, at + 2)?).ok()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SOI, JFIF, a DQT, then SOF0 of 400x300.
    const HEADER: &str = "/9j/4AAQSkZJRgABAQAAAQABAAD/2wAFAAEC/8AAEQgBLAGQAwEiAAIRAQMRAQ==";

    #[test]
    fn jpeg_size_is_read_past_other_segments() {
        assert_eq!(jpeg_size(HEADER), Some((400, 300)));
        let frame = Frame::new(HEADER.to_owned(), 200, 150);
        assert_eq!(
            (frame.w, frame.h, frame.css_w, frame.css_h),
            (400, 300, 200, 150)
        );
        assert_eq!(frame.b64, HEADER, "passed through untouched");
    }

    #[test]
    fn the_wire_form_is_a_header_and_the_decoded_jpeg() {
        let frame = Frame {
            seq: 0x0102_0304_0506_0708,
            ..Frame::new(HEADER.to_owned(), 200, 150)
        };
        let wire = frame.wire().unwrap();
        assert_eq!(wire[..8], 0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(wire[8..12], 200u32.to_le_bytes());
        assert_eq!(wire[12..16], 150u32.to_le_bytes());
        assert_eq!(wire[16..], STANDARD.decode(HEADER).unwrap());
        let broken = Frame::new("not base64!".to_owned(), 1, 1);
        assert_eq!(broken.wire(), None);
    }

    #[test]
    fn truncated_or_foreign_data_falls_back_to_the_css_size() {
        assert_eq!(
            jpeg_size(&HEADER[..30]),
            None,
            "cut before the frame header"
        );
        assert_eq!(jpeg_size("iVBORw0KGgoAAAANSUhEUgAA"), None, "a PNG");
        assert_eq!(jpeg_size(""), None);
        let frame = Frame::new(String::new(), 200, 150);
        assert_eq!((frame.w, frame.h), (200, 150));
    }

    #[test]
    fn proportions_are_checked_against_the_css_size() {
        let frame = |w, h, css_w, css_h| Frame {
            b64: String::new(),
            w,
            h,
            css_w,
            css_h,
            seq: 0,
        };
        assert!(frame(800, 600, 400, 300).has_css_proportions());
        assert!(frame(1280, 657, 1280, 657).has_css_proportions());
        assert!(frame(801, 600, 400, 300).has_css_proportions(), "rounding");
        assert!(
            !frame(400, 205, 400, 300).has_css_proportions(),
            "window surface under the panel's metadata"
        );
    }
}
