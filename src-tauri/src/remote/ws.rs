//! One phone WebSocket: handshake over the already-parsed upgrade request,
//! authentication, then a single thread that alternates between reading
//! (short socket timeout) and writing what the hub queued. The TLS stream
//! cannot be split, hence no separate writer thread; the hub never waits on
//! this thread.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::error::Error as WsError;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use super::devices::{Device, DeviceStore};
use super::emit::Action;
use super::http::Head;
use super::hub::{Hub, Out, CLOSE_NOT_READY, CLOSE_UNAUTHORIZED};
use super::push_subs::parse_subscription;
use super::server::{Conn, Transport, UpgradeFn};

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an idle connection looks at its outbound queue.
const POLL: Duration = Duration::from_millis(50);
/// Longest one read call may take once authenticated, however steadily the
/// peer sends: a trickled frame cannot keep the thread from its outbox.
const FRAME_READ: Duration = Duration::from_secs(3);
/// Longest a started inbound frame may stay incomplete.
const FRAME_LIFETIME: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Inbound frame limit.
const MAX_FRAME: usize = 1 << 20;
/// Limit for an action's `text`.
const MAX_TEXT: usize = 32 * 1024;
/// The auth frame is tiny; anything bigger before auth is not a client.
const MAX_AUTH_FRAME: usize = 4096;
/// Time to write a close frame to a peer that is not reading.
const CLOSE_WRITE: Duration = Duration::from_secs(2);
const OUTBOX: usize = 64;
const MAX_ID_LEN: usize = 128;
const MAX_ASK_ID_LEN: usize = 256;
const CLOSE_TOO_BIG: u16 = 1009;
const CLOSE_BINARY: u16 = 1003;
const CLOSE_OVERLOAD: u16 = 1013;
const KINDS: [&str; 4] = ["prompt", "steer", "abort", "answer"];

/// `generation` is the hub's generation when the server started: a socket
/// that authenticates after the hub was closed since is refused.
pub fn upgrade_fn(hub: Arc<Hub>, devices: Arc<DeviceStore>, generation: u64) -> UpgradeFn {
    Arc::new(move |conn, head| serve(&hub, &devices, generation, conn, &head))
}

type Socket = WebSocket<Transport>;

fn close_with(ws: &mut Socket, code: u16) {
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

fn serve(hub: &Hub, devices: &DeviceStore, generation: u64, mut conn: Conn, head: &Head) {
    let auth_deadline = Instant::now() + AUTH_TIMEOUT;
    let key = head.header("sec-websocket-key");
    let version_ok = head.header("sec-websocket-version") == Some("13");
    let (Some(key), true) = (key, version_ok) else {
        let _ = conn.stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        conn.stream.close();
        return;
    };
    let accept = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        derive_accept_key(key.as_bytes())
    );
    let written = conn.stream.write_all(accept.as_bytes()).is_ok() && conn.stream.flush().is_ok();
    // Until the peer authenticates, all its I/O shares one deadline: a client
    // trickling bytes cannot keep the socket (and its in-flight slot) alive.
    conn.stream.set_deadline(Some(auth_deadline));
    conn.stream.set_caps(Some(POLL), Some(WRITE_TIMEOUT));
    if !written || conn.tcp.set_nodelay(true).is_err() {
        return;
    }
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_AUTH_FRAME))
        .max_frame_size(Some(MAX_AUTH_FRAME));
    let mut ws = WebSocket::from_raw_socket(conn.stream, Role::Server, Some(config));
    // Held until this thread is done with the socket, not just until the
    // peer authenticates: the connection limits count physical sockets.
    let _slot = conn.slot.take();
    if hub.is_ready() {
        run(hub, devices, generation, &mut ws, &conn.tcp, auth_deadline);
    } else {
        close_with(&mut ws, CLOSE_NOT_READY);
    }
    ws.get_mut().close();
    drain(&conn.tcp);
}

/// Let the peer read our close frame: closing a socket with unread input
/// would reset it.
fn drain(tcp: &std::net::TcpStream) {
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

pub(super) fn is_idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

enum Step {
    Continue,
    Close(u16),
    Stop,
}

struct Authed {
    conn: u64,
    device: Device,
    rx: Receiver<Out>,
}

/// Serve one socket until it ends; `auth_deadline` bounds authentication.
fn run(
    hub: &Hub,
    devices: &DeviceStore,
    generation: u64,
    ws: &mut Socket,
    tcp: &TcpStream,
    auth_deadline: Instant,
) {
    let mut authed: Option<Authed> = None;
    let mut frame_since: Option<Instant> = None;
    loop {
        let step = if let Some(session) = &authed {
            hub.expire(session.conn, Instant::now());
            match flush_outbox(ws, session) {
                Step::Continue => read_step(hub, ws, session, &mut frame_since),
                other => other,
            }
        } else if Instant::now() >= auth_deadline {
            Step::Close(CLOSE_UNAUTHORIZED)
        } else {
            ws.get_mut().set_deadline(Some(auth_deadline));
            match read_text(ws) {
                Ok(Frame::Idle | Frame::Skipped) => Step::Continue,
                Ok(Frame::Text(text)) => match authenticate(hub, devices, generation, tcp, &text) {
                    Ok(session) => {
                        ws.get_mut().set_deadline(None);
                        ws.set_config(|config| {
                            *config = config
                                .max_message_size(Some(MAX_FRAME))
                                .max_frame_size(Some(MAX_FRAME));
                        });
                        authed = Some(session);
                        Step::Continue
                    }
                    Err(code) => Step::Close(code),
                },
                Err(step) => step,
            }
        };
        match step {
            Step::Continue => {}
            Step::Close(code) => {
                close_with(ws, code);
                break;
            }
            Step::Stop => break,
        }
    }
    if let Some(session) = authed {
        hub.unregister(session.conn);
    }
}

fn authenticate(
    hub: &Hub,
    devices: &DeviceStore,
    generation: u64,
    tcp: &TcpStream,
    text: &str,
) -> Result<Authed, u16> {
    let message: Value = serde_json::from_str(text).map_err(|_| CLOSE_UNAUTHORIZED)?;
    let (Some("auth"), Some(Value::String(token))) = (
        message.get("t").and_then(Value::as_str),
        message.get("token"),
    ) else {
        return Err(CLOSE_UNAUTHORIZED);
    };
    let device = devices.authenticate(token).ok_or(CLOSE_UNAUTHORIZED)?;
    let (tx, rx) = mpsc::sync_channel(OUTBOX);
    // The hub cuts this handle when it gives up on the connection.
    let cut = tcp.try_clone().map_err(|_| CLOSE_OVERLOAD)?;
    let conn = hub.register(&device, tx, Some(cut), generation)?;
    Ok(Authed { conn, device, rx })
}

enum Frame {
    Text(String),
    /// A control frame or nothing we act on.
    Skipped,
    /// The socket had nothing (more) to read; a frame may be half received.
    Idle,
}

fn flush_control(ws: &mut Socket, restore_deadline: Option<Instant>) {
    ws.get_mut()
        .set_deadline(Some(Instant::now() + CLOSE_WRITE));
    let _ = ws.flush();
    ws.get_mut().set_deadline(restore_deadline);
}

fn read_text(ws: &mut Socket) -> Result<Frame, Step> {
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

fn flush_outbox(ws: &mut Socket, session: &Authed) -> Step {
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
fn read_step(
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

fn text_field(message: &Value, key: &str) -> Option<String> {
    message.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn handle(hub: &Hub, session: &Authed, text: &str) {
    let Ok(message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    match message.get("t").and_then(Value::as_str) {
        Some("select") => {
            if let Some(action) = build_action(session, &message, "select") {
                hub.action(session.conn, &action);
            }
        }
        Some("action") => {
            let kind = text_field(&message, "kind").filter(|kind| KINDS.contains(&kind.as_str()));
            let req_id = text_field(&message, "reqId").unwrap_or_default();
            let action = kind.and_then(|kind| build_action(session, &message, &kind));
            match action {
                Some(action) => hub.action(session.conn, &action),
                None => reject(hub, session, &req_id, "bad request"),
            }
        }
        Some("push-subscribe") => {
            let result = parse_subscription(message.get("sub"))
                .and_then(|sub| hub.subscribe(&session.device.id, sub));
            hub.push_ack(session.conn, &result);
        }
        Some("push-unsubscribe") => {
            hub.push_ack(session.conn, &hub.unsubscribe(&session.device.id));
        }
        _ => {}
    }
}

/// Answer a refused request through the hub's own ack path.
fn reject(hub: &Hub, session: &Authed, req_id: &str, error: &str) {
    if req_id.is_empty() {
        return;
    }
    hub.reject(session.conn, req_id, error);
}

fn build_action(session: &Authed, message: &Value, kind: &str) -> Option<Action> {
    let req_id =
        text_field(message, "reqId").filter(|id| !id.is_empty() && id.len() <= MAX_ID_LEN)?;
    let tab_id =
        text_field(message, "tabId").filter(|id| !id.is_empty() && id.len() <= MAX_ID_LEN)?;
    let text = text_field(message, "text").filter(|text| text.len() <= MAX_TEXT);
    if message.get("text").is_some_and(|raw| !raw.is_null()) && text.is_none() {
        return None;
    }
    let ask_id = text_field(message, "askId").filter(|id| id.len() <= MAX_ASK_ID_LEN);
    if message.get("askId").is_some_and(|raw| !raw.is_null()) && ask_id.is_none() {
        return None;
    }
    let value = message.get("value").cloned(); // owned for the webview event
    if value
        .as_ref()
        .is_some_and(|value| value.to_string().len() > MAX_TEXT)
    {
        return None;
    }
    Some(Action {
        req_id,
        device_id: session.device.id.clone(), // the action outlives the session borrow
        tab_id,
        kind: kind.to_owned(),
        text,
        ask_id,
        value,
        dispatched_at: None,
    })
}
