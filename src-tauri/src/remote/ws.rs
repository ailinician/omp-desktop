//! One phone WebSocket: handshake over the already-parsed upgrade request,
//! authentication, then a single thread that alternates between reading
//! (short socket timeout) and writing what the hub queued. The TLS stream
//! cannot be split, hence no separate writer thread; the hub never waits on
//! this thread.

use std::io::{self, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::{Role, WebSocketConfig};
use tungstenite::WebSocket;

mod frames;
mod messages;
use frames::{close_with, drain, flush_outbox, read_step, read_text, Frame, CLOSE_OVERLOAD};

use super::devices::{Device, DeviceStore};
use super::http::Head;
use super::hub::{Hub, Out, CLOSE_NOT_READY, CLOSE_UNAUTHORIZED};
use super::server::{Conn, Transport, UpgradeFn};

const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// How often an idle connection looks at its outbound queue.
const POLL: Duration = Duration::from_millis(50);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Inbound frame limit.
pub(super) const MAX_FRAME: usize = 1 << 20;
/// The auth frame is tiny; anything bigger before auth is not a client.
const MAX_AUTH_FRAME: usize = 4096;
pub(super) const OUTBOX: usize = 64;

/// `generation` is the hub's generation when the server started: a socket
/// that authenticates after the hub was closed since is refused.
pub fn upgrade_fn(hub: Arc<Hub>, devices: Arc<DeviceStore>, generation: u64) -> UpgradeFn {
    Arc::new(move |conn, head| serve(&hub, &devices, generation, conn, &head))
}

pub(super) type Socket = WebSocket<Transport>;

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

pub(super) fn is_idle(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted
    )
}

pub(super) enum Step {
    Continue,
    Close(u16),
    Stop,
}

pub(super) struct Authed {
    pub conn: u64,
    pub device: Device,
    pub rx: Receiver<Out>,
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
