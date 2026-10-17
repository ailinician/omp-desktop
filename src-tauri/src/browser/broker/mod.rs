//! The one CDP connection to a tab's Chromium: its debugging pipe
//! (`--remote-debugging-pipe`, NUL-terminated JSON both ways), shared by
//! every client — the panel's [`CdpClient`](super::cdp::CdpClient) and each
//! agent connection the proxy accepts.
//!
//! Chromium listens on no port at all, so nothing reaches the browser except
//! through this broker. Each client gets a session of its own on the pipe
//! (`Target.attachToBrowserTarget`, or `Target.attachToTarget` for a
//! page-level client) and sees it as a connection of its own: `route.rs`
//! adds and removes that session, gives each command a pipe-wide id and its
//! reply the client's id back, and keeps clients to the sessions they own.
//! Messages from Chromium are routed on one reader thread into each client's
//! bounded [`Outbox`]; a client that falls too far behind is cut. Messages
//! to Chromium wait in a bounded queue of their own for one writer thread,
//! so no sender ever blocks on the pipe. Neither thread keeps the broker
//! alive: dropping the last handle closes it.

mod outbox;
mod route;

#[cfg(test)]
pub(super) mod fake;

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

pub use outbox::Recv;
use outbox::{Outbox, Refused};
use route::{error_reply, inner_session, ClientId, Head, Inbound, Routes};

use super::lock;

/// Longest message read from Chromium. Its own receive limit is 100 MiB; a
/// full-page screenshot of a long page comes close.
const MAX_MESSAGE: u64 = 256 << 20;

/// Longest message a client may send. Chromium's pipe reader ends the whole
/// pipe on a message over 100 MiB, so a longer one is answered here with an
/// error and never sent; the margin leaves room for the id and `sessionId`
/// the broker writes in.
pub const MAX_COMMAND: usize = (100 << 20) - (64 << 10);

/// Capacity the reader's buffer keeps between messages; one huge message
/// does not pin its size for good.
const READ_BUFFER_KEPT: usize = 1 << 20;

/// How long attaching a client may take.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(10);

type Waiters = Mutex<HashMap<u64, Sender<Result<Value, String>>>>;

#[derive(Default)]
struct State {
    routes: Routes,
    outboxes: HashMap<ClientId, Arc<Outbox>>,
    /// Clients being attached, by the id of the broker's attach command;
    /// the reader adds each when it routes a successful reply.
    attaching: HashMap<u64, (ClientId, Arc<Outbox>)>,
    next_client: ClientId,
}

struct Threads {
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
}

pub struct Broker {
    /// For the `/devtools/browser/<id>` URL the proxy hands out.
    id: String,
    /// Messages for Chromium, in order, for the writer thread.
    to_chromium: Arc<Outbox>,
    state: Mutex<State>,
    /// The broker's own root-session commands in flight.
    waiters: Waiters,
    /// Ids on the pipe: the broker's own commands and every client's.
    next_id: AtomicU64,
    alive: AtomicBool,
    threads: Mutex<Option<Threads>>,
}

impl Broker {
    /// Serve the pipe pair: `from_chromium` is read and `to_chromium`
    /// written on threads of their own, until the pipe ends or the broker
    /// is closed (or dropped).
    pub fn start(
        from_chromium: impl Read + Send + 'static,
        to_chromium: impl Write + Send + 'static,
    ) -> io::Result<Arc<Self>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let queue = Arc::new(Outbox::default());
        let broker = Arc::new(Self {
            id: format!(
                "{:08x}-{:x}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            to_chromium: Arc::clone(&queue),
            state: Mutex::new(State::default()),
            waiters: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            alive: AtomicBool::new(true),
            threads: Mutex::new(None),
        });
        let reading = Arc::downgrade(&broker);
        let reader = thread::Builder::new()
            .name("browser-cdp-pipe".into())
            .spawn(move || Self::read_loop(&reading, BufReader::new(from_chromium)))?;
        let writing = Arc::downgrade(&broker);
        let writer = thread::Builder::new()
            .name("browser-cdp-write".into())
            .spawn(move || Self::write_loop(&writing, &queue, to_chromium))?;
        *lock(&broker.threads) = Some(Threads { reader, writer });
        Ok(broker)
    }

    /// The browser id in the `/devtools/browser/<id>` URL.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Whether the pipe still works.
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// A command on the broker's own root session (discovery, attaching
    /// clients), waiting up to `timeout` for its result.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, String> {
        self.request(method, params)?
            .wait(timeout)
            .unwrap_or_else(|| Err(format!("{method}: timed out")))
    }

    /// Send a root-session command; its result is the [`Pending`]'s.
    pub fn request(&self, method: &str, params: Value) -> Result<Pending<'_>, String> {
        self.send_root(method, params, None)
    }

    /// Send a root-session command; with `attach`, the client the reader
    /// adds when the command succeeds.
    fn send_root(
        &self,
        method: &str,
        params: Value,
        attach: Option<(ClientId, Arc<Outbox>)>,
    ) -> Result<Pending<'_>, String> {
        let id = self.next_id();
        let (tx, rx) = mpsc::channel();
        lock(&self.waiters).insert(id, tx);
        let attaching = attach.is_some();
        if let Some(client) = attach {
            lock(&self.state).attaching.insert(id, client);
        }
        // Dropped on an error below, it forgets the command again.
        let pending = Pending {
            broker: self,
            id,
            attaching,
            rx,
        };
        // `close` clears `alive` before it fails the waiters: either it sees
        // this entry or we see the flag.
        if !self.is_alive() {
            return Err("browser connection closed".into());
        }
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        self.enqueue(message)?;
        Ok(pending)
    }

    /// A browser-level client: a fresh browser session of its own.
    pub fn connect_browser(self: &Arc<Self>) -> Result<BrokerClient, String> {
        self.attach("Target.attachToBrowserTarget", json!({}))
    }

    /// A page-level client: a fresh session on `target_id`.
    pub fn connect_page(self: &Arc<Self>, target_id: &str) -> Result<BrokerClient, String> {
        self.attach(
            "Target.attachToTarget",
            json!({ "targetId": target_id, "flatten": true }),
        )
    }

    /// A client on the session `method` attaches. The reader adds it while
    /// it routes the reply, before any later message: a
    /// `Target.detachedFromTarget` right behind the reply finds the client
    /// and ends it.
    fn attach(self: &Arc<Self>, method: &str, params: Value) -> Result<BrokerClient, String> {
        let outbox = Arc::new(Outbox::default());
        let id = {
            let mut state = lock(&self.state);
            state.next_client += 1;
            state.next_client
        };
        let attached = self
            .send_root(method, params, Some((id, Arc::clone(&outbox))))?
            .wait(ATTACH_TIMEOUT)
            .unwrap_or_else(|| Err(format!("{method}: timed out")));
        // Dropped on an error below, it removes whatever the reader added.
        let client = BrokerClient {
            broker: Arc::clone(self),
            id,
            outbox,
        };
        if attached?["sessionId"].as_str().is_none() {
            return Err("attach reply has no sessionId".into());
        }
        if !self.is_alive() {
            return Err("browser connection closed".into());
        }
        Ok(client)
    }

    /// Forget `client`; detach its root session (and with it every session
    /// it attached) when the browser still runs.
    fn remove(&self, client: ClientId) {
        let mut state = lock(&self.state);
        let root = state.routes.remove_client(client);
        let outbox = state.outboxes.remove(&client);
        drop(state);
        if let Some(outbox) = outbox {
            outbox.close();
        }
        if let Some(root) = root.filter(|_| self.is_alive()) {
            let detach = json!({
                "id": self.next_id(),
                "method": "Target.detachFromTarget",
                "params": { "sessionId": root },
            });
            let _ = self.enqueue(detach.to_string());
        }
    }

    /// Queue `message` for the writer thread.
    fn enqueue(&self, message: String) -> Result<(), String> {
        self.to_chromium.offer(message).map_err(|refused| {
            let why = match refused {
                Refused::Closed => "browser connection closed",
                Refused::Full => "browser pipe: too much waiting to be sent",
            };
            why.to_owned()
        })
    }

    /// End the connection: every client and pending call ends, the writer
    /// stops and the pipe threads are woken. Idempotent, and callable from
    /// any thread. Chromium itself is the caller's to stop.
    pub fn close(&self) {
        if !self.alive.swap(false, Ordering::SeqCst) {
            return;
        }
        // Dropping the senders wakes every waiting `call`.
        lock(&self.waiters).clear();
        let outboxes: Vec<_> = {
            let mut state = lock(&self.state);
            state.attaching.clear();
            state.outboxes.drain().map(|(_, o)| o).collect()
        };
        for outbox in outboxes {
            outbox.close();
        }
        // The writer ends with its queue and drops our end of the pipe.
        self.to_chromium.close();
        let threads = lock(&self.threads);
        if let Some(threads) = threads.as_ref() {
            let ours = [&threads.reader, &threads.writer];
            let me = thread::current().id();
            // On a pipe thread the flags are enough: it leaves when this
            // call returns.
            if ours.iter().all(|t| t.thread().id() != me) {
                super::process::cancel_blocked_read(&ours);
            }
        }
    }

    fn read_loop(broker: &Weak<Self>, mut from: impl BufRead) {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            buf.shrink_to(READ_BUFFER_KEPT);
            match (&mut from).take(MAX_MESSAGE + 1).read_until(0, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) if buf.last() != Some(&0) => break, // over the cap, or cut off
                Ok(_) => {}
            }
            buf.pop();
            let Some(live) = broker.upgrade() else {
                return;
            };
            if !live.is_alive() {
                return;
            }
            match String::from_utf8(std::mem::take(&mut buf)) {
                Ok(text) => live.route(text),
                Err(not_text) => buf = not_text.into_bytes(),
            }
        }
        if let Some(live) = broker.upgrade() {
            live.close();
        }
    }

    fn write_loop(broker: &Weak<Self>, queue: &Outbox, mut to: impl Write) {
        while let Some(message) = queue.next_until_closed() {
            let written = to
                .write_all(message.as_bytes())
                .and_then(|()| to.write_all(b"\0"))
                .and_then(|()| to.flush());
            if written.is_err() {
                if let Some(live) = broker.upgrade() {
                    live.close();
                }
                return;
            }
        }
    }

    fn route(&self, text: String) {
        let mut state = lock(&self.state);
        let inbound = state.routes.inbound(&text);
        match inbound {
            Inbound::Deliver { client, rewritten } => {
                let Some(outbox) = state.outboxes.get(&client) else {
                    return;
                };
                if !outbox.push(rewritten.unwrap_or(text)) {
                    // Overflowed: the client is cut and its sessions go.
                    drop(state);
                    self.remove(client);
                }
            }
            Inbound::Root => self.root_message(state, &text),
            Inbound::Drop => {}
        }
    }

    /// A reply to the broker's own call, or an event of its root session.
    fn root_message(&self, mut state: MutexGuard<'_, State>, text: &str) {
        let Some(head) = Head::parse(text) else {
            return;
        };
        if let Some(id) = head.id {
            if !state.attaching.is_empty() {
                if let Some((client, outbox)) = state.attaching.remove(&id) {
                    // An error reply has no `result`: the client is not
                    // added. `close` drains the outboxes after clearing
                    // `alive`, so it sees this one or we see the flag.
                    if let Some(root) = inner_session(head.result).filter(|_| self.is_alive()) {
                        state.routes.add_client(client, root);
                        state.outboxes.insert(client, outbox);
                    }
                }
            }
            drop(state);
            let waiter = lock(&self.waiters).remove(&id);
            if let Some(waiter) = waiter {
                let _ = waiter.send(root_result(text));
            }
        } else if head.method.as_deref() == Some("Target.detachedFromTarget") {
            // A client's root went away (its page closed): so does the client.
            let client = inner_session(head.params)
                .and_then(|session| state.routes.client_of_root(&session));
            drop(state);
            if let Some(client) = client {
                self.remove(client);
            }
        }
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.close();
    }
}

/// A root-session command sent with [`Broker::request`]. Dropping it
/// forgets the command; a later reply is dropped.
pub struct Pending<'a> {
    broker: &'a Broker,
    id: u64,
    /// Whether a client waits in `attaching` on its reply.
    attaching: bool,
    rx: Receiver<Result<Value, String>>,
}

impl Pending<'_> {
    /// The command's result, or `None` when it has not come within
    /// `timeout` (wait again for more).
    pub fn wait(&self, timeout: Duration) -> Option<Result<Value, String>> {
        match self.rx.recv_timeout(timeout) {
            Ok(result) => Some(result),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err("browser connection closed".into())),
        }
    }
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        lock(&self.broker.waiters).remove(&self.id);
        if self.attaching {
            lock(&self.broker.state).attaching.remove(&self.id);
        }
    }
}

/// The result of a root reply, or its error message.
fn root_result(text: &str) -> Result<Value, String> {
    let mut reply: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if let Some(error) = reply.get("error") {
        return Err(error["message"].as_str().unwrap_or("cdp error").to_owned());
    }
    Ok(reply["result"].take())
}

/// One client's connection through the broker. Dropping it detaches the
/// client's sessions.
pub struct BrokerClient {
    broker: Arc<Broker>,
    id: ClientId,
    outbox: Arc<Outbox>,
}

impl BrokerClient {
    /// Send one CDP message as the client wrote it. A message the broker
    /// refuses (not JSON, no integer id, a session the client does not own,
    /// over [`MAX_COMMAND`]) is answered with an error reply in the
    /// client's outbox, as Chromium would. `Err` when the connection is
    /// gone, or too much waits to be written to the pipe already.
    pub fn send(&self, text: &str) -> Result<(), String> {
        if self.outbox.is_closed() {
            return Err("browser connection closed".into());
        }
        if text.len() > MAX_COMMAND {
            let head = Head::parse(text);
            let head = head.as_ref();
            self.outbox.push(error_reply(
                head.and_then(|h| h.id),
                head.and_then(|h| h.session.as_deref()),
                -32600,
                "Message is too long",
            ));
            return Ok(());
        }
        let broker = &self.broker;
        let wire = broker.next_id();
        let routed = lock(&broker.state).routes.outbound(self.id, text, wire);
        match routed {
            Ok(message) => broker
                .enqueue(message)
                .inspect_err(|_| lock(&broker.state).routes.forget(wire)),
            Err(reply) => {
                self.outbox.push(reply);
                Ok(())
            }
        }
    }

    /// The next message from Chromium for this client.
    pub fn recv(&self, timeout: Duration) -> Recv {
        self.outbox.recv(timeout)
    }

    /// End this client. Idempotent; a waiting [`recv`](Self::recv) returns
    /// [`Recv::Closed`].
    pub fn close(&self) {
        self.outbox.close();
        self.broker.remove(self.id);
    }
}

impl Drop for BrokerClient {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests;
