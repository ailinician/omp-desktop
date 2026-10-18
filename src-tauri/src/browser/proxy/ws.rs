//! An agent's CDP WebSocket, ended at the proxy and pumped both ways through
//! its [`BrokerClient`]: client text messages to the broker, the broker's
//! messages back as text frames. Each is pushed to its direction's
//! [`Feed`] before it is forwarded. A binary message is passed on as text
//! when it is UTF-8 (CDP is JSON); otherwise the connection is closed with
//! 1003 (unsupported data). A message over [`MAX_COMMAND`], which Chromium
//! would not take, fails this connection alone.
//!
//! A tungstenite `WebSocket` cannot be split, so the connection's thread
//! reads and a second thread writes, one at a time under the socket's
//! mutex. The reader waits for data with `peek` on a second handle, without
//! the mutex, so a writer never queues behind a blocked read; only the
//! non-blocking drain of what arrived runs under it.

use std::io::{self, ErrorKind, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use tungstenite::error::Error as WsError;
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Message, Utf8Bytes, WebSocket};

use super::feed::Feed;
use crate::browser::broker::{BrokerClient, Recv, MAX_COMMAND};
use crate::browser::lock;

/// How long an idle pump waits at a time (the reader's `peek`, the
/// writer's `recv`).
const POLL: Duration = Duration::from_millis(100);
/// How long a write to the agent may block before the connection is cut.
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);

type Socket = Mutex<WebSocket<TcpStream>>;

/// Accepts the upgrade on `stream` (its head, with `key`, vetted already;
/// `leftover` are the bytes read past it) and pumps until either side ends.
/// `feeds` are the client's and the server's.
pub(super) fn serve(
    mut stream: TcpStream,
    key: &str,
    leftover: Vec<u8>,
    client: &BrokerClient,
    feeds: (Feed, Feed),
) -> io::Result<()> {
    // One write: the agent reads the whole answer at once.
    let accept = format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
        derive_accept_key(key.as_bytes())
    );
    stream.write_all(accept.as_bytes())?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    // Bounds the reader's `peek`; the WebSocket itself only ever reads
    // non-blocking.
    stream.set_read_timeout(Some(POLL))?;
    let socket = stream.try_clone()?;
    // Larger messages could only be refused by the broker; tungstenite
    // fails the connection on them before buffering more.
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_COMMAND))
        .max_frame_size(Some(MAX_COMMAND));
    let ws = Mutex::new(WebSocket::from_partially_read(
        stream,
        leftover,
        Role::Server,
        Some(config),
    ));
    let (up_feed, down_feed) = feeds;
    let (ws, socket) = (&ws, &socket);
    thread::scope(|scope| {
        let down = thread::Builder::new()
            .name("browser-proxy-down".into())
            .spawn_scoped(scope, move || downstream(ws, socket, client, &down_feed));
        if down.is_ok() {
            upstream(ws, socket, client, &up_feed);
        }
        // Either side ending ends both: downstream's `recv` returns
        // `Closed` once the client is closed, a write to the shut-down
        // socket fails.
        client.close();
        let _ = socket.shutdown(Shutdown::Both);
        down.map(|_| ())
    })
}

/// Agent to broker, until the agent closes, the broker is gone or
/// downstream shuts the socket down (its `peek` then fails).
fn upstream(ws: &Socket, socket: &TcpStream, client: &BrokerClient, feed: &Feed) {
    let mut probe = [0u8; 1];
    // Frames that came in with the request head are buffered already.
    let mut buffered = true;
    loop {
        if !buffered {
            match socket.peek(&mut probe) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    continue
                }
                Err(_) => return,
            }
        }
        buffered = false;
        let (messages, alive) = drain(ws);
        for text in &messages {
            feed.push(text);
            if client.send(text).is_err() {
                return;
            }
        }
        if !alive {
            return;
        }
    }
}

/// Read every complete message that has arrived, without blocking (the
/// socket mutex is held, so the socket is non-blocking only meanwhile).
/// Returns the texts and whether the connection is still usable.
fn drain(ws: &Socket) -> (Vec<Utf8Bytes>, bool) {
    let mut ws = lock(ws);
    let mut messages = Vec::new();
    if ws.get_ref().set_nonblocking(true).is_err() {
        return (messages, false);
    }
    let mut closing = false;
    let alive = loop {
        match ws.read() {
            Ok(Message::Text(text)) => messages.push(text),
            Ok(Message::Binary(bytes)) => {
                if let Ok(text) = Utf8Bytes::try_from(bytes) {
                    messages.push(text);
                } else {
                    let _ = ws.close(Some(CloseFrame {
                        code: CloseCode::Unsupported,
                        reason: Utf8Bytes::from_static("CDP messages are UTF-8 JSON"),
                    }));
                    closing = true;
                    break false;
                }
            }
            Ok(Message::Close(_)) => {
                closing = true;
                break false;
            }
            // Tungstenite answers pings itself.
            Ok(_) => {}
            Err(WsError::Io(e)) if e.kind() == ErrorKind::WouldBlock => break true,
            Err(_) => break false,
        }
    };
    let blocking = ws.get_ref().set_nonblocking(false).is_ok();
    if closing && blocking {
        // The close frame tungstenite queued, in reply or of its own.
        let _ = ws.flush();
    }
    drop(ws);
    (messages, alive && blocking)
}

/// Broker to agent, until the broker ends this client (or upstream's end
/// closes it) or a write fails.
fn downstream(ws: &Socket, socket: &TcpStream, client: &BrokerClient, feed: &Feed) {
    loop {
        match client.recv(POLL) {
            Recv::Message(text) => {
                feed.push(&text);
                if lock(ws).send(Message::text(text)).is_err() {
                    break;
                }
            }
            Recv::Timeout => {}
            Recv::Closed => break,
        }
    }
    let _ = socket.shutdown(Shutdown::Both);
}
