//! A small Chrome `DevTools` Protocol client on one connection through the
//! tab's [`Broker`](super::broker::Broker).
//!
//! One reader thread per connection receives and dispatches both responses
//! (to the waiting [`CdpClient::call`]) and events (to the handler given to
//! [`CdpClient::connect`]). Sending goes straight to the broker from any
//! thread, never behind the reader.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use serde_json::{json, Value};

use super::broker::{BrokerClient, Recv};
use super::lock;

/// How long [`CdpClient::call`] waits for the response.
const CALL_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the idle reader looks at the shutdown flag.
const POLL: Duration = Duration::from_millis(100);

/// Delivered to the event handler (without a session) once when the
/// connection ends without [`CdpClient::close`] having been called.
pub const DISCONNECTED: &str = "CDP.disconnected";

/// Handler of events: session id, method, params (owned, so a large field
/// such as a screencast frame moves on without a copy). Runs on the reader
/// thread, so it must be quick and must not [`call`](CdpClient::call) (the
/// response could only be read by the thread that is waiting for it);
/// [`notify`](CdpClient::notify) is fine.
pub type EventHandler = Box<dyn Fn(Option<&str>, &str, Value) + Send>;

type Waiters = Mutex<HashMap<u64, Sender<Result<Value, String>>>>;

struct Shared {
    client: BrokerClient,
    waiters: Waiters,
    next_id: AtomicU64,
    closed: AtomicBool,
}

pub struct CdpClient {
    shared: Arc<Shared>,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl CdpClient {
    /// Serve `client` (from [`Broker::connect_browser`] or
    /// [`Broker::connect_page`]), reporting its events to `on_event`.
    ///
    /// [`Broker::connect_browser`]: super::broker::Broker::connect_browser
    /// [`Broker::connect_page`]: super::broker::Broker::connect_page
    pub fn connect(client: BrokerClient, on_event: EventHandler) -> Result<Self, String> {
        let shared = Arc::new(Shared {
            client,
            waiters: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("cdp-reader".into())
            .spawn(move || read_loop(&reader_shared, &on_event))
            .map_err(|e| format!("cdp reader thread: {e}"))?;
        Ok(Self {
            shared,
            reader: Mutex::new(Some(reader)),
        })
    }

    /// Send a command and wait up to 10 s for its response.
    pub fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<Value, String> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        lock(&self.shared.waiters).insert(id, tx);
        // The reader sets `closed` before it fails the waiters, so either it
        // sees this entry or we see the flag.
        if self.shared.closed.load(Ordering::SeqCst) {
            lock(&self.shared.waiters).remove(&id);
            return Err("cdp connection closed".into());
        }
        if let Err(e) = self.send(id, session, method, params) {
            lock(&self.shared.waiters).remove(&id);
            return Err(e);
        }
        match rx.recv_timeout(CALL_TIMEOUT) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                lock(&self.shared.waiters).remove(&id);
                Err(format!("cdp {method}: timed out"))
            }
            Err(RecvTimeoutError::Disconnected) => Err("cdp connection closed".into()),
        }
    }

    /// Send a command without waiting for the response (it is discarded).
    /// For the event handler, which must not block on the reader.
    pub fn notify(&self, session: Option<&str>, method: &str, params: Value) {
        let _ = self.post(session, method, params);
    }

    /// [`notify`](Self::notify) that says whether the command went out.
    pub fn post(&self, session: Option<&str>, method: &str, params: Value) -> Result<(), String> {
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err("cdp connection closed".into());
        }
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        self.send(id, session, method, params)
    }

    fn send(
        &self,
        id: u64,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(), String> {
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::from(session);
        }
        self.shared
            .client
            .send(&message.to_string())
            .map_err(|e| format!("cdp send: {e}"))
    }

    /// End the connection and the reader thread. Idempotent.
    pub fn close(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        // Wakes a reader waiting in `recv`.
        self.shared.client.close();
        let reader = lock(&self.reader).take();
        if let Some(reader) = reader {
            // The last handle to the client can drop on the reader thread
            // itself (through the event handler); it cannot join itself.
            if reader.thread().id() != thread::current().id() {
                let _ = reader.join();
            }
        }
    }
}

impl Drop for CdpClient {
    fn drop(&mut self) {
        self.close();
    }
}

fn read_loop(shared: &Shared, on_event: &EventHandler) {
    while !shared.closed.load(Ordering::SeqCst) {
        match shared.client.recv(POLL) {
            Recv::Message(text) => dispatch(shared, on_event, &text),
            Recv::Timeout => {}
            Recv::Closed => break,
        }
    }
    let lost = !shared.closed.swap(true, Ordering::SeqCst);
    // Dropping the senders wakes every waiting `call`.
    lock(&shared.waiters).clear();
    if lost {
        on_event(None, DISCONNECTED, Value::Null);
    }
}

fn dispatch(shared: &Shared, on_event: &EventHandler, text: &str) {
    let Ok(mut message) = serde_json::from_str::<Value>(text) else {
        return;
    };
    if let Some(id) = message.get("id").and_then(Value::as_u64) {
        let waiter = lock(&shared.waiters).remove(&id);
        if let Some(waiter) = waiter {
            let error = message.get("error").map(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("cdp error")
                    .to_owned()
            });
            let result = error.map_or_else(|| Ok(message["result"].take()), Err);
            let _ = waiter.send(result);
        }
    } else if message.get("method").is_some_and(Value::is_string) {
        let params = message["params"].take();
        let method = message["method"].as_str().unwrap_or_default();
        let session = message.get("sessionId").and_then(Value::as_str);
        on_event(session, method, params);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::thread;
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};

    use super::super::tests::{cdp_error, cdp_result, fake_cdp};
    use super::{CdpClient, DISCONNECTED};

    /// A one-client CDP stand-in: `Target.attachToTarget` attaches session
    /// `s1`, `echo` answers with its params, `event` sends an event first,
    /// `fail` answers with an error, `drop` ends the browser connection
    /// without answering.
    fn fake_server() -> super::super::broker::BrokerClient {
        fake_cdp(|request| match request["method"].as_str() {
            Some("Target.attachToTarget") => {
                Some(vec![cdp_result(request, json!({ "sessionId": "s1" }))])
            }
            Some("event") => Some(vec![
                json!({ "method": "Fake.event", "sessionId": "s1", "params": { "n": 1 } }),
                cdp_result(request, json!({})),
            ]),
            Some("fail") => Some(vec![cdp_error(request, "nope")]),
            Some("drop") => None,
            _ => Some(vec![cdp_result(request, request["params"].clone())]), // the reply owns its result
        })
    }

    type Seen = (Option<String>, String, Value);

    fn connect() -> (CdpClient, Receiver<Seen>) {
        let (tx, rx) = mpsc::channel();
        let client = CdpClient::connect(
            fake_server(),
            Box::new(move |session, method, params| {
                let _ = tx.send((session.map(str::to_owned), method.to_owned(), params));
            }),
        )
        .unwrap();
        (client, rx)
    }

    /// Attach session `s1`, which the client then owns.
    fn attach(client: &CdpClient) {
        let attached = client
            .call(None, "Target.attachToTarget", json!({ "targetId": "t" }))
            .unwrap();
        assert_eq!(attached["sessionId"], "s1");
    }

    #[test]
    fn calls_get_their_own_responses_and_events_reach_the_handler() {
        let (client, events) = connect();
        assert_eq!(
            client.call(None, "echo", json!({ "a": 1 })).unwrap(),
            json!({ "a": 1 })
        );
        attach(&client);
        client.call(Some("s1"), "event", json!({})).unwrap();
        let (session, method, params) = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(session.as_deref(), Some("s1"));
        assert_eq!(method, "Fake.event");
        assert_eq!(params["n"], 1);
        client.close();
    }

    #[test]
    fn concurrent_calls_are_matched_by_id() {
        let (client, _events) = connect();
        thread::scope(|s| {
            let calls: Vec<_> = (0..8)
                .map(|i| {
                    let client = &client;
                    s.spawn(move || client.call(None, "echo", json!({ "i": i })).unwrap())
                })
                .collect();
            for (i, call) in calls.into_iter().enumerate() {
                assert_eq!(call.join().unwrap(), json!({ "i": i }));
            }
        });
    }

    #[test]
    fn an_error_response_is_the_calls_error() {
        let (client, _events) = connect();
        assert_eq!(client.call(None, "fail", json!({})), Err("nope".to_owned()));
    }

    #[test]
    fn a_post_is_sent_without_waiting_for_the_reply() {
        let (client, events) = connect();
        attach(&client);
        assert_eq!(client.post(Some("s1"), "event", json!({})), Ok(()));
        let (_, method, _) = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, "Fake.event", "the command reached the server");
        client.close();
        assert!(
            client.post(None, "echo", json!({})).is_err(),
            "a closed connection is an error"
        );
    }

    #[test]
    fn a_lost_connection_fails_the_pending_call_and_is_reported_once() {
        let (client, events) = connect();
        let started = Instant::now();
        assert!(client.call(None, "drop", json!({})).is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "not the 10 s timeout"
        );
        let (_, method, _) = events.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(method, DISCONNECTED);
        assert!(client.call(None, "echo", json!({})).is_err());
        assert!(events.recv_timeout(Duration::from_millis(300)).is_err());
    }

    #[test]
    fn close_ends_the_reader_promptly_without_a_disconnect_event() {
        let (client, events) = connect();
        client.call(None, "echo", json!({})).unwrap();
        let started = Instant::now();
        client.close();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(client.call(None, "echo", json!({})).is_err());
        assert!(events.recv_timeout(Duration::from_millis(300)).is_err());
        client.close(); // idempotent
    }
}
