//! TLS (or, by explicit switch, plain) HTTP server: static phone page,
//! `POST /pair`, and the WebSocket upgrade hook.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};
use serde::Deserialize;

use super::assets;
use super::devices::DeviceStore;
use super::http::{read_body, read_head, write_response, Allowed, Head, HttpError, Response};
use super::pairing::{PairError, Pairing};

/// `POST /pair` bodies are tiny JSON; anything bigger is refused.
const MAX_PAIR_BODY: usize = 4096;
/// The phone sends the first 120 characters of its user agent; longer names
/// are cut, not refused.
const MAX_DEVICE_NAME_CHARS: usize = 64;
/// Live connections, all peers together, from accept to the end of the
/// connection thread (an upgraded WebSocket included): each holds a thread
/// and two descriptors, and macOS defaults to 256 descriptors per process.
const MAX_IN_FLIGHT: usize = 48;
/// The same, per peer address (the hub admits at most 8 authenticated sockets, the rest is headroom for sockets still authenticating).
const MAX_IN_FLIGHT_PER_IP: usize = 16;
/// How often the accept loop looks at its stop flag.
const ACCEPT_POLL: Duration = Duration::from_millis(10);

/// Accepted connection after the handshake-free part: the transport plus a
/// handle on the raw socket for timeouts.
pub struct Conn {
    pub stream: Transport,
    pub tcp: TcpStream,
    /// The connection slot of the physical socket, held until the WebSocket
    /// thread is done with it, so these limits also bound sockets the hub
    /// has already given up on.
    pub slot: Option<Slot>,
}

pub enum Transport {
    Tls(Box<StreamOwned<ServerConnection, DeadlineSock>>),
    Plain(DeadlineSock),
}

impl Transport {
    /// Best-effort orderly shutdown (TLS `close_notify`, then TCP FIN).
    pub fn close(&mut self) {
        match self {
            Self::Tls(stream) => {
                stream.conn.send_close_notify();
                let _ = stream.flush();
                let _ = stream.sock.tcp.shutdown(Shutdown::Write);
            }
            Self::Plain(sock) => {
                let _ = sock.tcp.shutdown(Shutdown::Write);
            }
        }
    }

    /// Bytes received on the socket so far.
    pub const fn bytes_read(&self) -> u64 {
        match self {
            Self::Tls(stream) => stream.sock.bytes_read,
            Self::Plain(sock) => sock.bytes_read,
        }
    }

    fn sock(&mut self) -> &mut DeadlineSock {
        match self {
            Self::Tls(stream) => &mut stream.sock,
            Self::Plain(sock) => sock,
        }
    }

    /// Bound all further socket I/O (TLS handshake included) by `deadline`;
    /// `None` leaves only the per-call caps.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.sock().deadline = deadline;
    }

    /// Longest a single read / write may block, within any deadline.
    pub fn set_caps(&mut self, read: Option<Duration>, write: Option<Duration>) {
        let sock = self.sock();
        sock.read_cap = read;
        sock.write_cap = write;
    }

    fn is_handshaking(&self) -> bool {
        matches!(self, Self::Tls(stream) if stream.conn.is_handshaking())
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.read(buf),
            Self::Plain(sock) => sock.read(buf),
        }
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.write(buf),
            Self::Plain(sock) => sock.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.flush(),
            Self::Plain(sock) => sock.flush(),
        }
    }
}

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

/// The socket under the transport. `rustls::Stream::read` (and a trickling
/// peer) may make many socket reads, so a per-read timeout is no deadline:
/// every call re-arms the socket with the time left, and fails once it is
/// spent. The caps bound a single call within that (the idle poll).
pub struct DeadlineSock {
    tcp: TcpStream,
    deadline: Option<Instant>,
    read_cap: Option<Duration>,
    write_cap: Option<Duration>,
    /// Bytes received so far, to tell an idle socket from a trickling one.
    bytes_read: u64,
}

impl DeadlineSock {
    const fn new(tcp: TcpStream, deadline: Instant) -> Self {
        Self {
            tcp,
            deadline: Some(deadline),
            read_cap: None,
            write_cap: None,
            bytes_read: 0,
        }
    }

    /// Timeout for the next call: the time left to the deadline, capped.
    fn timeout(&self, cap: Option<Duration>) -> io::Result<Option<Duration>> {
        let left = match self.deadline {
            None => None,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Some(left)
            }
        };
        Ok(match (left, cap) {
            (Some(left), Some(cap)) => Some(left.min(cap)),
            (left, cap) => left.or(cap),
        })
    }
}

impl Read for DeadlineSock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(timeout) = self.timeout(self.read_cap)? {
            self.tcp.set_read_timeout(Some(timeout))?;
        }
        let read = self.tcp.read(buf)?;
        self.bytes_read += read as u64;
        Ok(read)
    }
}

impl Write for DeadlineSock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(timeout) = self.timeout(self.write_cap)? {
            self.tcp.set_write_timeout(Some(timeout))?;
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// Connections accepted but not yet upgraded, in total and per peer address.
struct InFlight {
    total: usize,
    per_ip: usize,
    peers: Mutex<HashMap<IpAddr, usize>>,
}

impl InFlight {
    fn new(total: usize, per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            total,
            per_ip,
            peers: Mutex::new(HashMap::new()),
        })
    }

    /// A slot for `ip`, or `None` when a cap is reached.
    fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Slot> {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mine = peers.get(&ip).copied().unwrap_or(0);
        if mine >= self.per_ip || peers.values().sum::<usize>() >= self.total {
            return None;
        }
        peers.insert(ip, mine + 1);
        drop(peers);
        Some(Slot {
            owner: Arc::clone(self),
            ip,
        })
    }
}

/// One in-flight connection, released on drop.
pub struct Slot {
    owner: Arc<InFlight>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut peers = self
            .owner
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = peers.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                peers.remove(&self.ip);
            }
        }
    }
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
    TcpListener::bind((ip, port)).map_err(|error| format!("listen on {ip}:{port}: {error}"))
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
    let name: String = request
        .name
        .trim()
        .chars()
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    if name.is_empty() {
        return Response::status(400);
    }
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
