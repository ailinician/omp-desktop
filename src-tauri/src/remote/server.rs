//! HTTPS server: static phone page,
//! `POST /pair`, and the WebSocket upgrade hook.

use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};

pub mod routes;
mod transport;
use super::devices::DeviceStore;
use super::http::admission::Allowed;
use super::http::response::write_response;
use super::http::Head;
use super::pairing::Pairing;
use routes::{serve, Outcome};
use transport::InFlight;
pub use transport::{Conn, DeadlineSock, Slot, Transport};

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
    pub tls: Arc<rustls::ServerConfig>,
    /// Time a client gets for TLS handshake, head and body.
    pub request_deadline: Duration,
}

pub(super) struct Context {
    pub allowed: Allowed,
    tls: Arc<rustls::ServerConfig>,
    pub shared: Shared,
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
    let Ok(connection) = ServerConnection::new(Arc::clone(&context.tls)) else {
        return;
    };
    let stream = Transport::new(StreamOwned::new(
        connection,
        DeadlineSock::new(tcp, deadline),
    ));
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

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;
