//! WebSocket frame reading, outbound queue flushing, deadline enforcement,
//! and close/drain mechanics.

use std::io::Read;
use std::net::TcpStream;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use tungstenite::error::Error as WsError;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::CloseFrame;
use tungstenite::Message;

use super::messages::handle;
use super::{is_idle, Authed, Socket, Step, MAX_FRAME, OUTBOX};
use crate::remote::hub::{Hub, Out};

/// Longest one read call may take once authenticated, however steadily the
/// peer sends: a trickled frame cannot keep the thread from its outbox.
const FRAME_READ: Duration = Duration::from_secs(3);
/// Longest a started inbound frame may stay incomplete.
const FRAME_LIFETIME: Duration = Duration::from_secs(5);
/// Time to write a close frame to a peer that is not reading.
const CLOSE_WRITE: Duration = Duration::from_secs(2);
const CLOSE_TOO_BIG: u16 = 1009;
const CLOSE_BINARY: u16 = 1003;
pub(super) const CLOSE_OVERLOAD: u16 = 1013;

pub(super) enum Frame {
    Text(String),
    /// A control frame or nothing we act on.
    Skipped,
    /// The socket had nothing (more) to read; a frame may be half received.
    Idle,
}

pub(super) fn close_with(ws: &mut Socket, code: u16) {
    // The auth deadline may be spent by now; the close frame still gets out.
    ws.get_mut()
        .set_deadline(Some(Instant::now() + CLOSE_WRITE));
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: "".into(),
    };
    let _ = ws.close(Some(frame));
    let _ = ws.flush();
}

/// Let the peer read our close frame: closing a socket with unread input
/// would reset it.
pub(super) fn drain(tcp: &TcpStream) {
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut sink = [0_u8; 4096];
    let mut total = 0_usize;
    let mut reader = tcp;
    while Instant::now() < deadline && total < 2 * MAX_FRAME {
        match reader.read(&mut sink) {
            Ok(0) => return,
            Ok(read) => total += read,
            Err(error) if is_idle(&error) => {}
            Err(_) => return,
        }
    }
}

fn flush_control(ws: &mut Socket, restore_deadline: Option<Instant>) {
    ws.get_mut()
        .set_deadline(Some(Instant::now() + CLOSE_WRITE));
    let _ = ws.flush();
    ws.get_mut().set_deadline(restore_deadline);
}

pub(super) fn read_text(ws: &mut Socket) -> Result<Frame, Step> {
    match ws.read() {
        Ok(Message::Text(text)) => Ok(Frame::Text(text.as_str().to_owned())),
        Ok(Message::Binary(_)) => Err(Step::Close(CLOSE_BINARY)),
        Ok(Message::Close(_)) => {
            flush_control(ws, None);
            Err(Step::Stop)
        }
        Ok(_) => {
            flush_control(ws, None);
            Ok(Frame::Skipped)
        }
        Err(WsError::Io(error)) if is_idle(&error) => Ok(Frame::Idle),
        Err(WsError::Capacity(_)) => Err(Step::Close(CLOSE_TOO_BIG)),
        Err(_) => Err(Step::Stop),
    }
}

pub(super) fn flush_outbox(ws: &mut Socket, session: &Authed) -> Step {
    for _ in 0..OUTBOX {
        match session.rx.try_recv() {
            Ok(Out::Text(text)) => {
                if ws.send(Message::text(text)).is_err() {
                    return Step::Stop;
                }
            }
            Ok(Out::Close(code)) => return Step::Close(code),
            Err(TryRecvError::Empty) => return Step::Continue,
            Err(TryRecvError::Disconnected) => return Step::Close(CLOSE_OVERLOAD),
        }
    }
    Step::Continue
}

/// Authenticated read. A frame that has started must finish within
/// `FRAME_LIFETIME` however it is trickled; each call is also bounded by
/// `FRAME_READ`, which only yields: the thread serves its outbox and the
/// partial frame stays buffered. Only the frame lifetime closes the socket.
pub(super) fn read_step(
    hub: &Hub,
    ws: &mut Socket,
    session: &Authed,
    frame_since: &mut Option<Instant>,
) -> Step {
    let started = Instant::now();
    let before = ws.get_ref().bytes_read();
    let deadline = frame_since.map_or(started + FRAME_READ, |since| {
        (since + FRAME_LIFETIME).min(started + FRAME_READ)
    });
    ws.get_mut().set_deadline(Some(deadline));
    let read = read_text(ws);
    ws.get_mut().set_deadline(None);
    match read {
        Ok(Frame::Text(text)) => {
            *frame_since = None;
            handle(hub, session, &text);
            Step::Continue
        }
        // Control frames must not start or renew an incomplete data message's deadline.
        Ok(Frame::Skipped) => match frame_since {
            Some(since) if since.elapsed() >= FRAME_LIFETIME => Step::Close(CLOSE_OVERLOAD),
            _ => Step::Continue,
        },
        Ok(Frame::Idle) => {
            if ws.get_ref().bytes_read() != before {
                frame_since.get_or_insert(started);
            }
            match frame_since {
                Some(since) if since.elapsed() >= FRAME_LIFETIME => Step::Close(CLOSE_OVERLOAD),
                _ => Step::Continue,
            }
        }
        Err(step) => step,
    }
}
