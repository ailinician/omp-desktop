//! Phone requests in flight: forwarded to the webview under a hub-unique
//! correlation id, answered by that id, expired after [`ACTION_TIMEOUT`].
//! The phone's own `reqId` only comes back in its ack: two phones may pick
//! the same one.

use std::time::Instant;

use serde_json::json;

use super::{Hub, Out, ACTION_TIMEOUT, MAX_PENDING_PER_CONN};
use crate::remote::emit::Action;

pub(super) struct Pending {
    pub conn: u64,
    /// The `reqId` the phone chose, echoed in its ack.
    req_id: String,
    deadline: Instant,
}

fn ack(req_id: &str, ok: bool, error: Option<&str>) -> Out {
    Out::Text(json!({ "t": "ack", "reqId": req_id, "ok": ok, "error": error }).to_string())
}

impl Hub {
    /// Forward a phone request to the webview and start its 15 s clock.
    /// Requests of a connection the hub no longer knows (revoked, closed)
    /// are dropped. `action.req_id` is the phone's and non-empty (the socket
    /// layer checks); the webview gets the action under a fresh hub id.
    pub fn action(&self, conn: u64, action: &Action) {
        let forward = self.with_state(|state| {
            if !state.conns.contains_key(&conn) {
                return None;
            }
            let mine = state.pending.values().filter(|p| p.conn == conn);
            let refusal = if mine.clone().any(|p| p.req_id == action.req_id) {
                Some("duplicate reqId")
            } else if mine.count() >= MAX_PENDING_PER_CONN {
                Some("too many pending requests")
            } else {
                None
            };
            if let Some(error) = refusal {
                state.send(conn, ack(&action.req_id, false, Some(error)));
                return None;
            }
            state.next_action += 1;
            let id = format!("a{}", state.next_action);
            state.pending.insert(
                id.clone(), // keyed for action_result / expire
                Pending {
                    conn,
                    req_id: action.req_id.clone(),
                    deadline: Instant::now() + ACTION_TIMEOUT,
                },
            );
            Some(Action {
                req_id: id,
                ..action.clone()
            })
        });
        if let Some(forwarded) = forward {
            self.emit.action(&forwarded);
        }
    }

    /// The webview's answer by hub id; unknown or already expired ids are
    /// ignored.
    pub fn action_result(&self, id: &str, ok: bool, error: Option<&str>) {
        self.with_state(|state| {
            if let Some(pending) = state.pending.remove(id) {
                state.send(pending.conn, ack(&pending.req_id, ok, error));
            }
        });
    }

    /// Refuse a request that never reached the webview.
    pub fn reject(&self, conn: u64, req_id: &str, error: &str) {
        self.with_state(|state| state.send(conn, ack(req_id, false, Some(error))));
    }

    /// Outcome of a push (un)subscribe, which has no request id:
    /// `{t:"ack", reqId:null, kind:"push", ok, error}`.
    pub fn push_ack(&self, conn: u64, result: &Result<(), String>) {
        let text = json!({
            "t": "ack", "reqId": null, "kind": "push",
            "ok": result.is_ok(), "error": result.as_ref().err(),
        })
        .to_string();
        self.with_state(|state| state.send(conn, Out::Text(text)));
    }

    /// Answer this connection's requests that the webview let time out.
    pub fn expire(&self, conn: u64, now: Instant) {
        self.with_state(|state| {
            let late: Vec<String> = state
                .pending
                .iter()
                .filter(|(_, pending)| pending.conn == conn && pending.deadline <= now)
                .map(|(id, _)| id.clone()) // removed from the map below
                .collect();
            for id in late {
                if let Some(pending) = state.pending.remove(&id) {
                    let error = Some("desktop did not answer");
                    state.send(conn, ack(&pending.req_id, false, error));
                }
            }
        });
    }
}
