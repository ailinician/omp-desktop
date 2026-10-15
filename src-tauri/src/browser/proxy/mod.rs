//! Placeholder port the agent connects to: a loopback proxy in front of the
//! lazily launched Chromium.
//!
//! Every request head is vetted first ([`http::admit`]: loopback `Host`, no
//! browser-only header, a WebSocket upgrade outside `/json*`), so nothing a
//! web page sends launches a browser. `/json*` responses are rewritten so
//! the CDP WebSocket URLs point back at the proxy; everything else (the
//! WebSocket upgrade and its frames) is spliced byte for byte, with a
//! read-only [`Tap`] looking along (see `feed.rs`, `tap.rs`).

mod feed;
mod http;
mod tap;
mod ws_frames;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use feed::Feed;
use tap::Tap;

/// Returns the live Chromium port, launching or relaunching it as needed.
pub type Ensure = Arc<dyn Fn() -> Result<u16, String> + Send + Sync>;

/// Told the id of a page the agent worked in. Runs on a tap observer thread:
/// it must be quick and must not block.
pub type AgentTarget = Arc<dyn Fn(&str) + Send + Sync>;

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);
/// Connections served at once; one more is refused on the accept thread
/// without a thread of its own. An agent opens a handful.
const MAX_CONNECTIONS: usize = 32;
/// How long the accept thread may spend refusing one connection.
const REFUSE_TIMEOUT: Duration = Duration::from_millis(100);

struct Shared {
    ensure: Ensure,
    on_target: AgentTarget,
    port: u16,
    /// Total time a client gets to deliver its request head.
    head_timeout: Duration,
    stopped: AtomicBool,
    next_id: AtomicU64,
    /// Connections in flight (each holds a [`Permit`]).
    connections: AtomicUsize,
    /// `#[cfg(test)]`: how long the client-side feeds of this proxy's
    /// connections sleep in every push; the ordering test sets it on its
    /// own proxy alone.
    #[cfg(test)]
    client_push_delay_ms: AtomicU64,
    /// Sockets of connections in flight, keyed by connection id; an entry
    /// lives exactly as long as its connection thread.
    live: Mutex<HashMap<u64, Vec<TcpStream>>>,
}

impl Shared {
    fn live(&self) -> MutexGuard<'_, HashMap<u64, Vec<TcpStream>>> {
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
        live.entry(id).or_default().push(handle);
        true
    }

    fn untrack(&self, id: u64) {
        self.live().remove(&id);
    }
}

/// One of the `MAX_CONNECTIONS` slots, freed when dropped.
struct Permit(Arc<Shared>);

impl Permit {
    fn take(shared: &Arc<Shared>) -> Option<Self> {
        if shared.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            shared.connections.fetch_sub(1, Ordering::AcqRel);
            return None;
        }
        Some(Self(Arc::clone(shared)))
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
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
            #[cfg(test)]
            client_push_delay_ms: AtomicU64::new(0),
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

    /// Stops accepting and cuts every live client and upstream socket.
    pub fn shutdown(&self) {
        if self.shared.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        // Wake the blocked `accept`; it sees `stopped` and drops the listener.
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.shared.port));
        let _ = TcpStream::connect_timeout(&addr, WAKE_TIMEOUT);
        for stream in self.shared.live().drain().flat_map(|(_, v)| v) {
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
            Ok(mut client) => {
                let Some(permit) = Permit::take(shared) else {
                    let _ = client.set_write_timeout(Some(REFUSE_TIMEOUT));
                    let _ = http::respond(
                        &mut client,
                        "503 Service Unavailable",
                        "too many connections",
                    );
                    continue;
                };
                // A failed spawn drops `client` (closing the connection) and
                // the permit.
                let _ = thread::Builder::new()
                    .name("browser-proxy-conn".into())
                    .spawn(move || handle(&permit.0, client));
            }
            // Transient accept failure (e.g. fd exhaustion): back off.
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn handle(shared: &Shared, client: TcpStream) {
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    if shared.track(id, &client) {
        let _ = serve(shared, id, client);
    }
    shared.untrack(id);
}

fn serve(shared: &Shared, id: u64, mut client: TcpStream) -> io::Result<()> {
    let deadline = Instant::now() + shared.head_timeout;
    let Some((buf, head_len)) = http::read_head(&mut client, deadline)? else {
        return Ok(());
    };
    // Vet the head before the lazy launch and before rewriting Host: a web
    // page must neither start a Chromium nor bypass its DNS-rebinding and
    // Origin checks.
    let admitted = match http::admit(&buf[..head_len - 4]) {
        Ok(admitted) => admitted,
        Err(why) => return http::respond(&mut client, "403 Forbidden", why),
    };
    let port = match (shared.ensure)() {
        Ok(port) => port,
        Err(e) => return http::respond(&mut client, "503 Service Unavailable", &e),
    };
    let upstream = match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
        Ok(upstream) => upstream,
        Err(e) => return http::respond(&mut client, "502 Bad Gateway", &e.to_string()),
    };
    if !shared.track(id, &upstream) {
        return Ok(());
    }
    if admitted.discovery {
        return http::relay_json(&mut client, upstream, admitted.head, (port, shared.port))
            .or_else(|e| http::respond(&mut client, "502 Bad Gateway", &e.to_string()));
    }
    client.set_read_timeout(None)?;
    let tap = Tap::new(&buf[..head_len], Arc::clone(&shared.on_target));
    let (up_feed, down_feed) = feed::spawn(tap);
    #[cfg(test)]
    let up_feed = up_feed.with_delay_ms(shared.client_push_delay_ms.load(Ordering::Relaxed));
    splice(client, upstream, &buf, head_len, (up_feed, down_feed))
}

/// Forwards `pending` (the head, `head_len` bytes, and anything pipelined
/// behind it) upstream, then copies both directions until either side closes.
/// `feeds` are the client's and the server's: every chunk is pushed to its
/// feed *before* it is forwarded (see `feed.rs`).
fn splice(
    client: TcpStream,
    mut upstream: TcpStream,
    pending: &[u8],
    head_len: usize,
    feeds: (Feed, Feed),
) -> io::Result<()> {
    let (mut up_feed, down_feed) = feeds;
    up_feed.push(&pending[head_len..]);
    upstream.write_all(pending)?;
    let (down_from, down_to) = (upstream.try_clone()?, client.try_clone()?);
    let down = thread::Builder::new()
        .name("browser-proxy-down".into())
        .spawn(move || pipe(down_from, down_to, down_feed))?;
    pipe(client, upstream, up_feed);
    let _ = down.join();
    Ok(())
}

/// Copies `from` into `to`, pushing each chunk to `feed` first. A clean EOF
/// only half-closes: `to` gets a FIN while the opposite direction keeps
/// flowing until its own EOF. A copy error cuts both sockets.
fn pipe(mut from: TcpStream, mut to: TcpStream, mut feed: Feed) {
    let mut chunk = [0u8; 16 * 1024];
    let copied = loop {
        match from.read(&mut chunk) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                feed.push(&chunk[..n]);
                if let Err(e) = to.write_all(&chunk[..n]) {
                    break Err(e);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => break Err(e),
        }
    };
    if copied.is_ok() {
        let _ = to.shutdown(Shutdown::Write);
    } else {
        let _ = from.shutdown(Shutdown::Both);
        let _ = to.shutdown(Shutdown::Both);
    }
}
