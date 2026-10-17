//! Commands waiting for their reply, by id: the broker's own root-session
//! calls and the panel's [`CdpClient`](super::cdp::CdpClient) calls.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use super::lock;

/// A command's `result`, or its error's message.
pub type Reply = Result<Value, String>;

pub struct Waiters {
    senders: Mutex<HashMap<u64, Sender<Reply>>>,
    /// What a wait gets once the connection has ended.
    closed: &'static str,
}

impl Waiters {
    pub fn new(closed: &'static str) -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            closed,
        }
    }

    /// Wait for the reply to `id`. The owner registers before it checks its
    /// closed flag, and its closer sets that flag before it calls
    /// [`fail_all`](Self::fail_all): either the closer sees this entry or
    /// the check sees the flag.
    pub fn register(&self, id: u64) -> Waiter<'_> {
        let (tx, rx) = mpsc::channel();
        lock(&self.senders).insert(id, tx);
        Waiter {
            table: self,
            id,
            rx,
        }
    }

    /// Hand `reply` (the whole reply message) to the waiter of `id`, if one
    /// still waits.
    pub fn complete(&self, id: u64, reply: Value) {
        let waiter = lock(&self.senders).remove(&id);
        if let Some(waiter) = waiter {
            let _ = waiter.send(reply_result(reply));
        }
    }

    /// End every wait: dropping the senders wakes each.
    pub fn fail_all(&self) {
        lock(&self.senders).clear();
    }
}

/// One command's wait. Dropping it forgets the command; a later reply is
/// dropped.
pub struct Waiter<'a> {
    table: &'a Waiters,
    id: u64,
    rx: Receiver<Reply>,
}

impl Waiter<'_> {
    /// The reply, or `None` when it has not come within `timeout` (wait
    /// again for more).
    pub fn wait(&self, timeout: Duration) -> Option<Reply> {
        match self.rx.recv_timeout(timeout) {
            Ok(reply) => Some(reply),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => Some(Err(self.table.closed.to_owned())),
        }
    }
}

impl Drop for Waiter<'_> {
    fn drop(&mut self) {
        lock(&self.table.senders).remove(&self.id);
    }
}

/// The `result` of a reply message, or its error's message.
fn reply_result(mut reply: Value) -> Reply {
    if let Some(error) = reply.get("error") {
        return Err(error["message"].as_str().unwrap_or("cdp error").to_owned());
    }
    Ok(reply["result"].take())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;

    use super::{lock, Waiters};

    const NOW: Duration = Duration::ZERO;

    #[test]
    fn a_reply_reaches_its_waiter_as_its_result_or_error_message() {
        let table = Waiters::new("closed");
        let (ok, failed, bare) = (table.register(1), table.register(2), table.register(3));
        table.complete(2, json!({ "id": 2, "error": { "message": "nope" } }));
        table.complete(1, json!({ "id": 1, "result": { "x": 1 } }));
        table.complete(3, json!({ "id": 3, "error": {} }));
        assert_eq!(ok.wait(NOW), Some(Ok(json!({ "x": 1 }))));
        assert_eq!(failed.wait(NOW), Some(Err("nope".into())));
        assert_eq!(bare.wait(NOW), Some(Err("cdp error".into())));
        assert_eq!(
            ok.wait(NOW),
            Some(Err("closed".into())),
            "one reply each: nothing more comes"
        );
    }

    #[test]
    fn fail_all_ends_every_wait_and_a_dropped_wait_is_forgotten() {
        let table = Waiters::new("gone");
        let waiting = table.register(1);
        drop(table.register(2));
        assert_eq!(
            lock(&table.senders).len(),
            1,
            "the dropped wait is forgotten"
        );
        table.complete(2, json!({ "result": {} })); // nobody waits: dropped
        table.fail_all();
        assert_eq!(waiting.wait(NOW), Some(Err("gone".into())));
        drop(waiting);
        assert!(lock(&table.senders).is_empty());
    }
}
