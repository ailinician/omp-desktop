//! Socket registration, revocation, and server-generation lifecycle on [`super::Hub`].

use std::net::TcpStream;
use std::sync::mpsc::SyncSender;

use serde_json::json;

use super::state::Entry;
use super::{
    push_key_message, Hub, Out, CLOSE_NOT_READY, CLOSE_TOO_MANY, CLOSE_UNAUTHORIZED,
    MAX_PER_DEVICE, MAX_TOTAL,
};
use crate::remote::devices::Device;

impl Hub {
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
