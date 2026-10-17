//! Placeholder port the agent connects to: a loopback HTTP and WebSocket
//! endpoint in front of the lazily launched Chromium, which itself listens
//! on no port (its CDP runs over a pipe, see `broker/`).
//!
//! Every connection is checked first: only a socket of the OS user this app
//! runs as is served ([`peer`]); anyone else's is closed unanswered before
//! its request is read. At most [`MAX_PEER_CHECKS`] checks run at once (one
//! may run `lsof`); a connection that finds none free is closed unanswered
//! too. Only a connection that passed waits for its head among the
//! [`MAX_UNVETTED`], so another user's sockets never displace ours. Every
//! request head is then vetted ([`http::admit`]: loopback `Host`, no
//! browser-only header, a WebSocket upgrade outside `/json*`), so nothing a
//! web page sends launches a browser. `/json*` discovery is answered from
//! the broker ([`discovery`]) with WebSocket URLs pointing back at the
//! proxy; a WebSocket upgrade becomes a broker client of its own, pumped
//! message by message ([`ws`]) with a read-only [`Tap`] looking along (see
//! `feed.rs`, `tap.rs`).

mod discovery;
mod feed;
mod http;
mod peer;
mod tap;
mod ws;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use discovery::Answer;
use http::Admitted;
use tap::Tap;

use super::broker::Broker;

/// Returns the live Chromium's broker, launching or relaunching it as needed.
pub type Ensure = Arc<dyn Fn() -> Result<Arc<Broker>, String> + Send + Sync>;

/// Told the id of a page the agent worked in. Runs on a tap observer thread:
/// it must be quick and must not block.
pub type AgentTarget = Arc<dyn Fn(&str) + Send + Sync>;

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// Admitted connections served at once; one more gets a 503 once its head
/// is admitted. An agent opens a handful.
const MAX_CONNECTIONS: usize = 32;
/// Peer checks ([`peer::same_user`]) running at once, at most; a connection
/// that finds none free is closed unanswered. A check reads the kernel's
/// socket table, or on macOS runs `lsof`: anyone on the machine can open
/// connections, and must not make the app spawn processes without bound.
/// The agent's connections are checked one after another in milliseconds.
const MAX_PEER_CHECKS: usize = 8;
/// Connections of our OS user waiting for their request head, at most; one
/// more cuts the one that has waited longest. A connection only waits here
/// once its peer check passed, so other users' sockets never count. Our
/// user's idle sockets (a web page's preconnects too) must still not hold
/// the slots the agent needs: the agent sends its head the moment it
/// connects, and only connections that have sent nothing for a while are
/// cut.
const MAX_UNVETTED: usize = 32;

struct Shared {
    ensure: Ensure,
    on_target: AgentTarget,
    port: u16,
    /// Total time a client gets to deliver its request head.
    head_timeout: Duration,
    stopped: AtomicBool,
    next_id: AtomicU64,
    /// Admitted connections in flight (each holds a [`Permit`]).
    connections: AtomicUsize,
    /// Peer checks in flight (each holds a [`Permit`]).
    peer_checks: AtomicUsize,
    /// Connections of our OS user still waiting for their request head,
    /// oldest first.
    unvetted: Mutex<VecDeque<(u64, TcpStream)>>,
    /// `#[cfg(test)]`: how long the client-side feeds of this proxy's
    /// connections sleep in every push; the ordering test sets it on its
    /// own proxy alone.
    #[cfg(test)]
    client_push_delay_ms: AtomicU64,
    /// `#[cfg(test)]`: every peer of this proxy counts as another OS user's.
    #[cfg(test)]
    force_foreign: AtomicBool,
    /// The client socket of each connection in flight, keyed by connection
    /// id; an entry lives exactly as long as its connection thread.
    live: Mutex<HashMap<u64, TcpStream>>,
}

impl Shared {
    fn live(&self) -> MutexGuard<'_, HashMap<u64, TcpStream>> {
        super::lock(&self.live)
    }

    /// Registers `stream` under `id` so `shutdown` can cut it. A stream that
    /// arrives after shutdown is cut on the spot and `false` returned.
    fn track(&self, id: u64, stream: &TcpStream) -> bool {
        let Ok(handle) = stream.try_clone() else {
            return false;
        };
        let mut live = self.live();
        if self.stopped.load(Ordering::SeqCst) {
            let _ = stream.shutdown(Shutdown::Both);
            return false;
        }
        live.insert(id, handle);
        true
    }

    fn untrack(&self, id: u64) {
        self.live().remove(&id);
    }

    /// Registers `stream`, whose peer check passed, as waiting for its head,
    /// cutting the connection that has waited longest when `MAX_UNVETTED`
    /// already wait. `false` when the stream cannot be registered (it is
    /// then to be dropped).
    fn await_head(&self, id: u64, stream: &TcpStream) -> bool {
        let Ok(handle) = stream.try_clone() else {
            return false;
        };
        let mut waiting = super::lock(&self.unvetted);
        if waiting.len() >= MAX_UNVETTED {
            if let Some((_, oldest)) = waiting.pop_front() {
                let _ = oldest.shutdown(Shutdown::Both);
            }
        }
        waiting.push_back((id, handle));
        true
    }

    /// Connection `id` no longer waits for its head (or never will).
    fn head_done(&self, id: u64) {
        super::lock(&self.unvetted).retain(|(waiting, _)| *waiting != id);
    }
}

/// One of the slots a counter bounds (`MAX_CONNECTIONS`, `MAX_PEER_CHECKS`),
/// freed when dropped.
struct Permit<'a>(&'a AtomicUsize);

impl<'a> Permit<'a> {
    fn take(taken: &'a AtomicUsize, max: usize) -> Option<Self> {
        if taken.fetch_add(1, Ordering::AcqRel) >= max {
            taken.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(taken))
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub struct Proxy {
    shared: Arc<Shared>,
}

impl Proxy {
    /// Binds `127.0.0.1:0` and starts accepting. `on_target` is told the page
    /// the agent worked in.
    pub fn bind(ensure: Ensure, on_target: AgentTarget) -> io::Result<Self> {
        Self::bind_with(ensure, on_target, HEAD_TIMEOUT)
    }

    fn bind_with(
        ensure: Ensure,
        on_target: AgentTarget,
        head_timeout: Duration,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let shared = Arc::new(Shared {
            ensure,
            on_target,
            port: listener.local_addr()?.port(),
            head_timeout,
            stopped: AtomicBool::new(false),
            next_id: AtomicU64::new(0),
            connections: AtomicUsize::new(0),
            peer_checks: AtomicUsize::new(0),
            unvetted: Mutex::new(VecDeque::new()),
            #[cfg(test)]
            client_push_delay_ms: AtomicU64::new(0),
            #[cfg(test)]
            force_foreign: AtomicBool::new(false),
            live: Mutex::new(HashMap::new()),
        });
        let accept_shared = Arc::clone(&shared);
        thread::Builder::new()
            .name("browser-proxy-accept".into())
            .spawn(move || accept_loop(&listener, &accept_shared))?;
        Ok(Self { shared })
    }

    pub fn port(&self) -> u16 {
        self.shared.port
    }

    /// `#[cfg(test)]`: slows the client-side feeds of this proxy's
    /// connections: the ordering test uses it so no parallel test sees the
    /// delay.
    #[cfg(test)]
    fn set_client_push_delay_ms(&self, millis: u64) {
        self.shared
            .client_push_delay_ms
            .store(millis, Ordering::Relaxed);
    }

    /// Stops accepting and cuts every live client socket.
    pub fn shutdown(&self) {
        if self.shared.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the blocked `accept`; it sees `stopped` and drops the listener.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.shared.port));
        let _ = TcpStream::connect_timeout(&addr, WAKE_TIMEOUT);
        for (_, stream) in self.shared.live().drain() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn accept_loop(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stopped.load(Ordering::SeqCst) {
            return;
        }
        match stream {
            Ok(client) => {
                let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
                let conn_shared = Arc::clone(shared);
                // A failed spawn drops `client`, closing the connection.
                let _ = thread::Builder::new()
                    .name("browser-proxy-conn".into())
                    .spawn(move || handle(&conn_shared, id, client));
            }
            // Transient accept failure (e.g. fd exhaustion): back off.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle(shared: &Shared, id: u64, client: TcpStream) {
    if shared.track(id, &client) {
        let _ = serve(shared, id, client);
    }
    // `serve` may have returned between `await_head` and its own call.
    shared.head_done(id);
    shared.untrack(id);
}

/// Whether the peer of `client` is a socket of our OS user; `false` too
/// when `MAX_PEER_CHECKS` checks are running already.
fn same_user(shared: &Shared, client: &TcpStream) -> bool {
    let Some(_check) = Permit::take(&shared.peer_checks, MAX_PEER_CHECKS) else {
        return false;
    };
    #[cfg(test)]
    if shared.force_foreign.load(Ordering::SeqCst) {
        return false;
    }
    peer::same_user(client)
}

/// A response refusing a request: status line and body.
type Refusal = (&'static str, &'static str);

const NOT_FOUND: Refusal = ("404 Not Found", "no such endpoint");

/// What an admitted request asks for.
enum Route<'a> {
    Discovery(discovery::Request<'a>),
    /// A WebSocket to a browser-level client, or to the page `page`.
    Socket {
        page: Option<&'a str>,
        key: &'a str,
    },
}

/// Where `admitted` goes, decided before anything launches.
fn route<'a>(admitted: &Admitted<'a>) -> Result<Route<'a>, Refusal> {
    let target = admitted.target;
    if target.starts_with("/json") {
        return discovery::parse(admitted.method, target).map(Route::Discovery);
    }
    // `admit` lets nothing else through without a WebSocket upgrade.
    let path = target.split(['?', '#']).next().unwrap_or_default();
    let page = if path.starts_with("/devtools/browser/") {
        None
    } else {
        match path.strip_prefix("/devtools/page/") {
            Some(id) if !id.is_empty() => Some(id),
            _ => return Err(NOT_FOUND),
        }
    };
    if admitted.method != "GET" || admitted.ws_version != Some("13") {
        return Err((
            "400 Bad Request",
            "a WebSocket upgrade is a GET with Sec-WebSocket-Version: 13",
        ));
    }
    let key = admitted
        .ws_key
        .filter(|key| !key.is_empty())
        .ok_or(("400 Bad Request", "Sec-WebSocket-Key required"))?;
    Ok(Route::Socket { page, key })
}

fn serve(shared: &Shared, id: u64, mut client: TcpStream) -> io::Result<()> {
    // Another OS user's socket gets nothing, not even a response: before
    // its head is read, before anything launches, before it may wait among
    // the `MAX_UNVETTED`.
    if !same_user(shared, &client) || !shared.await_head(id, &client) {
        return Ok(());
    }
    let deadline = Instant::now() + shared.head_timeout;
    let head = http::read_head(&mut client, deadline);
    shared.head_done(id);
    let Some((mut head, head_len)) = head? else {
        return Ok(());
    };
    // What the client sent past its head (the first WebSocket frames).
    let leftover = head.split_off(head_len);
    // Vet and route the head before the lazy launch: a web page must not
    // start a Chromium, and nor must a request nothing would answer.
    let admitted = match http::admit(&head[..head_len - 4]) {
        Ok(admitted) => admitted,
        Err(why) => return http::respond(&mut client, "403 Forbidden", why),
    };
    let route = match route(&admitted) {
        Ok(route) => route,
        Err((status, why)) => return http::respond(&mut client, status, why),
    };
    let Some(_permit) = Permit::take(&shared.connections, MAX_CONNECTIONS) else {
        return http::respond(
            &mut client,
            "503 Service Unavailable",
            "too many connections",
        );
    };
    let broker = match (shared.ensure)() {
        Ok(broker) => broker,
        Err(e) => return http::respond(&mut client, "503 Service Unavailable", &e),
    };
    let (page, key) = match route {
        Route::Discovery(request) => {
            return match discovery::answer(&request, &broker, shared.port) {
                Ok(Answer::Json(body)) => http::respond_as(
                    &mut client,
                    "200 OK",
                    "application/json; charset=UTF-8",
                    &body.to_string(),
                ),
                Ok(Answer::Text(body)) => http::respond(&mut client, "200 OK", body),
                Ok(Answer::NotFound(body)) => http::respond(&mut client, "404 Not Found", &body),
                Err(e) => http::respond(&mut client, broker_failure(&broker), &e),
            }
        }
        Route::Socket { page, key } => (page, key),
    };
    let connected = page.map_or_else(|| broker.connect_browser(), |id| broker.connect_page(id));
    let broker_client = match connected {
        Ok(broker_client) => broker_client,
        Err(e) => return http::respond(&mut client, broker_failure(&broker), &e),
    };
    let tap = Tap::new(page, Arc::clone(&shared.on_target));
    let (up_feed, down_feed) = feed::spawn(tap);
    #[cfg(test)]
    let up_feed = up_feed.with_delay_ms(shared.client_push_delay_ms.load(Ordering::Relaxed));
    ws::serve(client, key, leftover, &broker_client, (up_feed, down_feed))
}

/// The status of a failed broker command: the browser went away (503), or
/// it refused (502).
fn broker_failure(broker: &Broker) -> &'static str {
    if broker.is_alive() {
        "502 Bad Gateway"
    } else {
        "503 Service Unavailable"
    }
}
