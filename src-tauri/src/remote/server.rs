//! TLS (or, by explicit switch, plain) HTTP server: static phone page,
//! `POST /pair`, and the WebSocket upgrade hook.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};
use serde::Deserialize;

mod transport;
use super::assets;
use super::devices::{normalize_device_name, DeviceStore};
use super::http::{read_body, read_head, write_response, Allowed, Head, HttpError, Response};
use super::pairing::{PairError, Pairing};
use transport::InFlight;
pub use transport::{Conn, DeadlineSock, Slot, Transport};

/// `POST /pair` bodies are tiny JSON; anything bigger is refused.
const MAX_PAIR_BODY: usize = 4096;
/// Live connections, all peers together, from accept to the end of the
/// connection thread (an upgraded WebSocket included): each holds a thread
/// and two descriptors, and macOS defaults to 256 descriptors per process.
const MAX_IN_FLIGHT: usize = 48;
/// The same, per peer address (the hub admits at most 8 authenticated sockets, the rest is headroom for sockets still authenticating).
const MAX_IN_FLIGHT_PER_IP: usize = 16;
/// How often the accept loop looks at its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Called with a validated (Host + Origin) WebSocket upgrade request; owns the connection.
pub type UpgradeFn = Arc<dyn Fn(Conn, Head) + Send + Sync>;

pub struct Shared {
    pub pairing: Arc<Pairing>,
    pub devices: Arc<DeviceStore>,
    /// False once the server this belongs to was replaced or stopped:
    /// pairing then refuses (503).
    pub is_current: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Called after a device was added.
    pub on_paired: Arc<dyn Fn() + Send + Sync>,
}

pub struct Listen {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub host_name: Option<String>,
    /// `None` serves plain HTTP (explicit `allow_http` switch).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Time a client gets for TLS handshake, head and body.
    pub request_deadline: Duration,
}

struct Context {
    allowed: Allowed,
    tls: Option<Arc<rustls::ServerConfig>>,
    shared: Shared,
    on_upgrade: UpgradeFn,
    in_flight: Arc<InFlight>,
    request_deadline: Duration,
}

pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// Bind exactly one interface address; the wildcard address is refused.
pub fn bind(ip: Ipv4Addr, port: u16) -> Result<TcpListener, String> {
    if ip.is_unspecified() {
        return Err("refusing to listen on all interfaces (0.0.0.0)".to_owned());
    }
    TcpListener::bind((ip, port)).map_err(|error| match error.kind() {
        std::io::ErrorKind::AddrInUse => format!("port {port} is in use"),
        _ => format!("listen on {ip}:{port}: {error}"),
    })
}

impl Server {
    pub fn start(listen: Listen, shared: Shared, on_upgrade: UpgradeFn) -> Result<Self, String> {
        let listener = bind(listen.ip, listen.port)?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("configure listener: {error}"))?;
        let addr = listener
            .local_addr()
            .map_err(|error| format!("read listen address: {error}"))?;
        let context = Arc::new(Context {
            allowed: Allowed {
                ip: listen.ip,
                port: addr.port(),
                host_name: listen.host_name,
                plain: listen.tls.is_none(),
            },
            tls: listen.tls,
            shared,
            on_upgrade,
            in_flight: InFlight::new(MAX_IN_FLIGHT, MAX_IN_FLIGHT_PER_IP),
            request_deadline: listen.request_deadline,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let accept_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("remote-accept".to_owned())
            .spawn(move || accept_loop(&listener, &accept_stop, &context))
            .map_err(|error| format!("start remote server thread: {error}"))?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub const fn local_addr(&self) -> SocketAddr {
        self.addr
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept_loop(listener: &TcpListener, stop: &AtomicBool, context: &Arc<Context>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((tcp, peer)) => {
                // Over a cap: close at once, the client retries.
                let Some(slot) = context.in_flight.admit(peer.ip()) else {
                    continue;
                };
                if tcp.set_nonblocking(false).is_err() {
                    continue;
                }
                let context = Arc::clone(context);
                // A failed spawn drops socket and slot; the client reconnects.
                let _ = std::thread::Builder::new()
                    .name("remote-conn".to_owned())
                    .spawn(move || handle(&context, tcp, slot));
            }
            Err(_) => std::thread::sleep(ACCEPT_POLL),
        }
    }
}

enum Outcome {
    Reply(Response),
    Upgrade(Head),
    Drop,
}

fn handle(context: &Context, tcp: TcpStream, slot: Slot) {
    let Ok(peer) = tcp.peer_addr() else { return };
    let Ok(socket) = tcp.try_clone() else { return };
    if tcp
        .set_write_timeout(Some(context.request_deadline))
        .is_err()
    {
        return;
    }
    let deadline = Instant::now() + context.request_deadline;
    let stream = match &context.tls {
        Some(config) => match ServerConnection::new(Arc::clone(config)) {
            Ok(connection) => Transport::Tls(Box::new(StreamOwned::new(
                connection,
                DeadlineSock::new(tcp, deadline),
            ))),
            Err(_) => return,
        },
        None => Transport::Plain(DeadlineSock::new(tcp, deadline)),
    };
    let mut conn = Conn {
        stream,
        tcp: socket,
        slot: None,
    };
    let outcome = serve(context, &mut conn, peer.ip(), deadline);
    // Writing the reply or the upgrade answer gets a fresh allowance.
    conn.stream
        .set_deadline(Some(Instant::now() + context.request_deadline));
    match outcome {
        Outcome::Reply(response) => {
            let _ = write_response(&mut conn.stream, &response);
            conn.stream.close();
        }
        Outcome::Upgrade(head) => {
            // Released by the websocket once the peer authenticates.
            conn.slot = Some(slot);
            (context.on_upgrade)(conn, head);
        }
        Outcome::Drop => {}
    }
}

const fn error_outcome(error: &HttpError) -> Outcome {
    match error {
        HttpError::TooLarge => Outcome::Reply(Response::status(431)),
        HttpError::Malformed => Outcome::Reply(Response::status(400)),
        HttpError::Timeout => Outcome::Reply(Response::status(408)),
        HttpError::Closed | HttpError::Io => Outcome::Drop,
    }
}

fn serve(context: &Context, conn: &mut Conn, peer: IpAddr, deadline: Instant) -> Outcome {
    let socket = &conn.tcp;
    let arm = |remaining: Duration| socket.set_read_timeout(Some(remaining));
    let (head, rest) = match read_head(&mut conn.stream, &arm, deadline) {
        Ok(parsed) => parsed,
        // A stalled or failed handshake gets no HTTP answer.
        Err(_) if conn.stream.is_handshaking() => return Outcome::Drop,
        Err(error) => return error_outcome(&error),
    };
    if !context.allowed.host_ok(&head) {
        return Outcome::Reply(Response::status(403));
    }
    if head.is_websocket_upgrade() {
        if head.method != "GET" || head.path != "/ws" {
            return Outcome::Reply(Response::status(404));
        }
        if !context.allowed.origin_ok(&head) {
            return Outcome::Reply(Response::status(403));
        }
        return Outcome::Upgrade(head);
    }
    match (head.method.as_str(), head.path.as_str()) {
        ("GET", path) => Outcome::Reply(static_asset(path)),
        ("POST", "/pair") => {
            // A browser POST from another site carries its own Origin.
            if head.header("origin").is_some() && !context.allowed.origin_ok(&head) {
                return Outcome::Reply(Response::status(403));
            }
            let length = match head.content_length() {
                Ok(Some(length)) if length <= MAX_PAIR_BODY => length,
                Ok(Some(_)) => return Outcome::Reply(Response::status(413)),
                Ok(None) => return Outcome::Reply(Response::status(411)),
                Err(error) => return error_outcome(&error),
            };
            match read_body(&mut conn.stream, &arm, deadline, rest, length) {
                Ok(body) => Outcome::Reply(pair(&context.shared, &body, peer)),
                Err(error) => error_outcome(&error),
            }
        }
        _ => Outcome::Reply(Response::status(405)),
    }
}

fn static_asset(path: &str) -> Response {
    assets::lookup(path).map_or_else(
        || Response::status(404),
        |asset| Response {
            status: 200,
            content_type: asset.content_type,
            body: std::borrow::Cow::Borrowed(asset.body),
            extra: &[],
        },
    )
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
    name: String,
}

/// HTTP status for a refused pairing code.
pub const fn pair_status(error: &PairError) -> u16 {
    match error {
        PairError::Invalid => 403,
        PairError::Expired => 410,
        PairError::RateLimited => 429,
    }
}

fn pair(shared: &Shared, body: &[u8], peer: IpAddr) -> Response {
    let Ok(request) = serde_json::from_slice::<PairRequest>(body) else {
        return Response::status(400);
    };
    let Ok(name) = normalize_device_name(&request.name) else {
        return Response::status(400);
    };
    if !(shared.is_current)() {
        return Response::status(503);
    }
    if let Err(error) = shared.pairing.redeem(&request.code, peer, Instant::now()) {
        let response = Response::status(pair_status(&error));
        return if error == PairError::RateLimited {
            response.with_extra(&[("Retry-After", "60")])
        } else {
            response
        };
    }
    match shared.devices.add(&name) {
        Ok((device, token)) => {
            (shared.on_paired)();
            Response::json(
                200,
                serde_json::json!({ "token": token, "deviceId": device.id })
                    .to_string()
                    .into_bytes(),
            )
        }
        Err(error) => {
            eprintln!("[omp-desktop] could not save paired device: {error}");
            Response::status(500)
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
