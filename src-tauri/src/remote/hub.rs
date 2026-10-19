//! Registry of authenticated phone sockets: what the desktop publishes goes
//! out to them, what they ask for goes to the webview and its answer comes
//! back by request id. No method blocks on a socket: every connection owns a
//! bounded channel that its own thread drains, and a connection whose
//! channel is full is dropped.

use std::collections::{HashMap, HashSet};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

mod actions;
use actions::Pending;

use super::devices::{Device, DeviceStore, PushSubscription};
use super::emit::RemoteEmit;
use super::tracker::tracker;

pub const MAX_PER_DEVICE: usize = 4;
pub const MAX_TOTAL: usize = 8;
pub const MAX_PUSH_PER_DEVICE: usize = 4;
pub const MAX_PENDING_PER_CONN: usize = 16;
pub const ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// Application close codes.
pub const CLOSE_UNAUTHORIZED: u16 = 4401;
pub const CLOSE_TOO_MANY: u16 = 4429;
pub const CLOSE_NOT_READY: u16 = 4503;

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct TabMeta {
    pub id: String,
    pub title: String,
    pub project: String,
}

#[derive(Deserialize)]
pub struct PublishPayload {
    pub tabs: Vec<TabMeta>,
    pub view: Option<Value>,
    pub theme: Option<Value>,
}

/// What a connection thread is asked to write.
pub enum Out {
    Text(String),
    Close(u16),
}

struct Entry {
    device_id: String,
    tx: SyncSender<Out>,
    /// The raw socket, to cut it when the hub gives up on the connection.
    tcp: Option<TcpStream>,
}

#[derive(Default)]
struct State {
    next_conn: u64,
    next_action: u64,
    ready: bool,
    /// Bumped by `close_all`: a connection of an older server generation is
    /// refused at `register`.
    generation: u64,
    conns: HashMap<u64, Entry>,
    /// Devices revoked while this hub runs: a socket that authenticated just
    /// before is refused at `register` without asking the store under the lock.
    revoked: HashSet<String>,
    tabs: Vec<TabMeta>,
    view: Option<Value>,
    theme: Option<Value>,
    /// Merged tabs as last broadcast; unchanged tabs are not re-sent.
    sent_tabs: Value,
    /// The VAPID public key phones subscribe with, while push is on.
    push_key: Option<String>,
    watched: Vec<String>,
    connected: Vec<String>,
    pending: HashMap<String, Pending>,
}

/// What changed for the webview while the state was locked.
struct Changes {
    watch: Option<Vec<String>>,
    connected: Option<Vec<String>>,
}

pub struct Hub {
    devices: Arc<DeviceStore>,
    emit: Arc<dyn RemoteEmit>,
    state: Mutex<State>,
}

fn push_key_message(key: Option<&str>) -> String {
    json!({ "t": "push-key", "key": key }).to_string()
}

impl State {
    fn merged_tabs(&self) -> Value {
        let live = tracker().snapshot();
        let tabs: Vec<Value> = self
            .tabs
            .iter()
            .map(|tab| {
                let (busy, asks) = live.get(&tab.id).copied().unwrap_or((false, 0));
                json!({
                    "id": tab.id, "title": tab.title, "project": tab.project,
                    "busy": busy, "hasAsk": asks > 0,
                })
            })
            .collect();
        Value::Array(tabs)
    }

    fn send(&mut self, conn: u64, out: Out) {
        let Some(entry) = self.conns.get(&conn) else {
            return;
        };
        match entry.tx.try_send(out) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => self.remove(conn),
        }
    }

    fn broadcast(&mut self, text: &str) {
        let mut dropped = false;
        self.conns.retain(|_, entry| {
            if entry.tx.try_send(Out::Text(text.to_owned())).is_ok() {
                true
            } else {
                if let Some(tcp) = entry.tcp.take() {
                    let _ = tcp.shutdown(Shutdown::Both);
                }
                dropped = true;
                false
            }
        });
        if dropped {
            self.pending
                .retain(|_, pending| self.conns.contains_key(&pending.conn));
        }
    }

    /// Forget the registration and physically cut the socket. Dropping only
    /// the sender cannot interrupt a reader stuck inside an incomplete frame.
    fn remove(&mut self, conn: u64) {
        if let Some(tcp) = self.conns.remove(&conn).and_then(|entry| entry.tcp) {
            let _ = tcp.shutdown(Shutdown::Both);
        }
        self.pending.retain(|_, pending| pending.conn != conn);
    }

    /// Queue a close frame and forget the connection: it gets no more
    /// messages and its late actions are ignored.
    fn close(&mut self, conn: u64, code: u16) {
        self.send(conn, Out::Close(code));
        self.remove(conn);
    }

    fn close_all(&mut self, code: u16) {
        for (_, entry) in self.conns.drain() {
            let _ = entry.tx.try_send(Out::Close(code));
            if let Some(tcp) = entry.tcp {
                let _ = tcp.shutdown(Shutdown::Both);
            }
        }
        self.pending.clear();
    }

    /// Broadcast the merged tab list if it differs from the last one sent.
    fn sync_tabs(&mut self) {
        let merged = self.merged_tabs();
        if merged != self.sent_tabs {
            let text = json!({ "t": "tabs", "tabs": merged }).to_string();
            self.sent_tabs = merged;
            self.broadcast(&text);
        }
    }

    fn connected_devices(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .conns
            .values()
            .map(|entry| entry.device_id.clone()) // owned for the caller
            .collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// Watch list and connected devices, each when it differs from the last
    /// one handed out.
    fn take_changes(&mut self) -> Changes {
        let connected = self.connected_devices();
        let connected_change = (connected != self.connected).then(|| {
            self.connected.clone_from(&connected);
            connected
        });
        let ids: Vec<String> = if self.conns.is_empty() {
            Vec::new()
        } else {
            // Owned ids for the emit call made after the lock is released.
            self.tabs.iter().map(|tab| tab.id.clone()).collect()
        };
        let watch = (ids != self.watched).then(|| {
            // The hub keeps its own copy to detect the next change.
            self.watched.clone_from(&ids);
            ids
        });
        Changes {
            watch,
            connected: connected_change,
        }
    }
}

impl Hub {
    pub fn new(devices: Arc<DeviceStore>, emit: Arc<dyn RemoteEmit>) -> Self {
        Self {
            devices,
            emit,
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Run `change` under the lock, then tell the webview what it changed
    /// (connected devices, watch list) with the lock released.
    fn with_state<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let (result, changes) = {
            let mut state = self.lock();
            let result = change(&mut state);
            (result, state.take_changes())
        };
        if let Some(ids) = changes.connected {
            self.emit.connections(&ids);
        }
        if let Some(ids) = changes.watch {
            self.emit.watch(&ids);
        }
        result
    }

    pub fn is_ready(&self) -> bool {
        self.lock().ready
    }

    /// The webview announces itself. Losing it closes every phone socket
    /// (4503): nobody is left to answer their actions.
    pub fn set_ready(&self, ready: bool) {
        self.with_state(|state| {
            if std::mem::replace(&mut state.ready, ready) && !ready {
                state.close_all(CLOSE_NOT_READY);
            }
        });
    }

    pub fn publish(&self, payload: PublishPayload) {
        self.with_state(|state| {
            state.tabs = payload.tabs;
            state.sync_tabs();
            if payload.view != state.view {
                state.view = payload.view;
                let text = json!({ "t": "view", "view": state.view }).to_string();
                state.broadcast(&text);
            }
            if payload.theme != state.theme {
                state.theme = payload.theme;
                let text = json!({ "t": "theme", "theme": state.theme }).to_string();
                state.broadcast(&text);
            }
        });
    }

    /// The title of a published tab.
    pub fn tab_title(&self, id: &str) -> Option<String> {
        let state = self.lock();
        state
            .tabs
            .iter()
            .find(|tab| tab.id == id)
            .map(|tab| tab.title.clone()) // owned for the push message
    }

    /// Set (or withdraw) the push key; phones already connected get the update.
    pub fn set_push_key(&self, key: Option<String>) {
        self.with_state(|state| {
            if state.push_key != key {
                state.broadcast(&push_key_message(key.as_deref()));
                state.push_key = key;
            }
        });
    }

    /// Re-merge the tracker's busy/question state into the tab list.
    pub fn refresh(&self) {
        self.with_state(State::sync_tabs);
    }

    /// Admit an authenticated socket and queue its first messages, or name
    /// the close code to refuse it with.
    pub fn register(
        &self,
        device: &Device,
        tx: SyncSender<Out>,
        tcp: Option<TcpStream>,
        generation: u64,
    ) -> Result<u64, u16> {
        self.with_state(|state| {
            if state.generation != generation {
                return Err(CLOSE_NOT_READY);
            }
            if !state.ready {
                return Err(CLOSE_NOT_READY);
            }
            // Revoke removes the device first and then closes its sockets, so
            // either this check fails or the revoke finds the entry.
            if state.revoked.contains(&device.id) {
                return Err(CLOSE_UNAUTHORIZED);
            }
            let mine = state
                .conns
                .values()
                .filter(|entry| entry.device_id == device.id)
                .count();
            if mine >= MAX_PER_DEVICE || state.conns.len() >= MAX_TOTAL {
                return Err(CLOSE_TOO_MANY);
            }
            state.next_conn += 1;
            let conn = state.next_conn;
            state.conns.insert(
                conn,
                Entry {
                    device_id: device.id.clone(), // owned by the entry
                    tx,
                    tcp,
                },
            );
            // Serialise everything from `state` first: `send` needs it mutably.
            let hello = json!({ "t": "hello", "deviceId": device.id, "name": device.name });
            let key = state
                .push_key
                .as_deref()
                .map(|key| push_key_message(Some(key)));
            let tabs = json!({ "t": "tabs", "tabs": state.merged_tabs() });
            let view = state
                .view
                .as_ref()
                .map(|view| json!({ "t": "view", "view": view }).to_string());
            let theme = state
                .theme
                .as_ref()
                .map(|theme| json!({ "t": "theme", "theme": theme }).to_string());
            state.send(conn, Out::Text(hello.to_string()));
            for text in key
                .into_iter()
                .chain([tabs.to_string()])
                .chain(view)
                .chain(theme)
            {
                state.send(conn, Out::Text(text));
            }
            Ok(conn)
        })
    }

    pub fn unregister(&self, conn: u64) {
        self.with_state(|state| state.remove(conn));
    }

    fn finish_push_change(&self, result: Result<(), String>) -> Result<(), String> {
        if result.is_ok() {
            self.emit.connections(&self.connected_devices());
        }
        result
    }

    /// Replace one subscription (same endpoint) or add it, up to the cap.
    pub fn subscribe(&self, device_id: &str, sub: PushSubscription) -> Result<(), String> {
        self.finish_push_change(
            self.devices
                .add_push(device_id, sub, MAX_PUSH_PER_DEVICE)
                .map_err(|err| {
                    if err == "remote device not found" {
                        "unknown device".to_owned()
                    } else {
                        err
                    }
                }),
        )
    }

    pub fn unsubscribe(&self, device_id: &str) -> Result<(), String> {
        self.finish_push_change(self.devices.set_push(device_id, Vec::new()))
    }

    /// Revoke a device and close its sockets (4401); they get no further
    /// messages and their late actions are dropped.
    pub fn revoke(&self, device_id: &str) -> bool {
        let known = self.devices.revoke(device_id);
        self.with_state(|state| {
            if known {
                state.revoked.insert(device_id.to_owned());
            }
            let ids: Vec<u64> = state
                .conns
                .iter()
                .filter(|(_, entry)| entry.device_id == device_id)
                .map(|(id, _)| *id)
                .collect();
            for id in ids {
                state.close(id, CLOSE_UNAUTHORIZED);
            }
        });
        known
    }

    /// The current server generation; a server captures it when it starts.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Close every phone socket with `code`: the server they came in on is
    /// gone. Connections still in flight on it are refused afterwards.
    pub fn close_all(&self, code: u16) {
        self.with_state(|state| {
            state.generation += 1;
            state.close_all(code);
        });
    }

    /// Ids of devices with at least one open socket.
    pub fn connected_devices(&self) -> Vec<String> {
        self.lock().connected_devices()
    }
}

#[cfg(test)]
impl Hub {
    /// Run `then` while this thread holds the state lock (for tests of what
    /// must not wait for it).
    pub(super) fn while_locked<R>(&self, then: impl FnOnce() -> R) -> R {
        let _held = self.lock();
        then()
    }
}
