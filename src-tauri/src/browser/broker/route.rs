//! Which client a message from Chromium belongs to, and the rewriting that
//! gives each client what looks like a CDP connection of its own.
//!
//! Every client has a *root* session (a browser session from
//! `Target.attachToBrowserTarget`, or a page session for a page-level
//! client) that it addresses without a `sessionId`; the broker adds the root
//! on the way to Chromium and removes it on the way back. Sessions the
//! client attaches itself (attach replies, `Target.attachedToTarget` events
//! on a session it owns) are its own. A client can only address sessions it
//! owns.
//!
//! Every command a client sends goes out with an id of the broker's own,
//! unique on the pipe, and its reply comes back with the client's id: so a
//! reply finds its sender even when Chromium sends it without a `sessionId`
//! (a session it no longer knows, a malformed command), and no two clients'
//! ids meet. Events on a client's own sessions pass unchanged.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;

use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{json, Value};

pub type ClientId = u64;

/// A JSON object's members, values kept raw.
type Members<'t> = BTreeMap<Cow<'t, str>, &'t RawValue>;

/// The top-level fields routing reads; the rest of the message stays raw.
#[derive(Deserialize)]
pub struct Head<'a> {
    #[serde(default)]
    pub id: Option<u64>,
    #[serde(rename = "sessionId", borrow, default)]
    pub session: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub method: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    pub params: Option<&'a RawValue>,
    #[serde(borrow, default)]
    pub result: Option<&'a RawValue>,
}

impl<'a> Head<'a> {
    pub fn parse(text: &'a str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }
}

/// A JSON string, borrowed from the text when it has no escapes.
#[derive(Deserialize)]
struct Text<'a>(#[serde(borrow)] Cow<'a, str>);

/// The `sessionId` inside a `params` or `result` object.
pub fn inner_session(raw: Option<&RawValue>) -> Option<String> {
    #[derive(Deserialize)]
    struct Field<'a> {
        #[serde(rename = "sessionId", borrow, default)]
        session: Option<Cow<'a, str>>,
    }
    let field: Field<'_> = serde_json::from_str(raw?.get()).ok()?;
    field.session.map(Cow::into_owned)
}

/// Commands whose reply names a session the sender then owns.
fn attaches(method: &str) -> bool {
    matches!(
        method,
        "Target.attachToTarget" | "Target.attachToBrowserTarget"
    )
}

/// Where a message from Chromium goes.
#[derive(Debug, PartialEq, Eq)]
pub enum Inbound {
    /// A reply or event of the broker's own root connection.
    Root,
    /// To `client`: as `rewritten`, or the text unchanged when `None`.
    Deliver {
        client: ClientId,
        rewritten: Option<String>,
    },
    /// A session nobody owns (any more), or not a CDP message.
    Drop,
}

/// A client command on its way to Chromium.
struct InFlight {
    client: ClientId,
    /// The id the client gave it.
    id: u64,
    /// Whether its reply names a session the client then owns.
    attach: bool,
}

#[derive(Default)]
pub struct Routes {
    roots: HashMap<ClientId, String>,
    /// Every session a client may address: its root and what it attached.
    owners: HashMap<String, ClientId>,
    /// Client commands not answered yet, by the id they went out with.
    in_flight: HashMap<u64, InFlight>,
}

impl Routes {
    pub fn add_client(&mut self, client: ClientId, root: String) {
        self.owners.insert(root.clone(), client); // both maps key by it
        self.roots.insert(client, root);
    }

    /// Forget `client`, every session it owned and its commands in flight;
    /// returns its root.
    pub fn remove_client(&mut self, client: ClientId) -> Option<String> {
        self.owners.retain(|_, owner| *owner != client);
        self.in_flight.retain(|_, sent| sent.client != client);
        self.roots.remove(&client)
    }

    /// Forget the command that was to go out as `wire` (it was not sent).
    pub fn forget(&mut self, wire: u64) {
        self.in_flight.remove(&wire);
    }

    /// The client whose root session is `session`.
    pub fn client_of_root(&self, session: &str) -> Option<ClientId> {
        self.roots
            .iter()
            .find_map(|(client, root)| (root == session).then_some(*client))
    }

    fn is_root(&self, client: ClientId, session: &str) -> bool {
        self.roots.get(&client).is_some_and(|root| root == session)
    }

    /// `text` from `client`, ready for Chromium with the id `wire`; `Err` is
    /// the error reply the client gets instead (the message is not sent).
    pub fn outbound(&mut self, client: ClientId, text: &str, wire: u64) -> Result<String, String> {
        let Ok(members) = serde_json::from_str::<Members<'_>>(text) else {
            return Err(error_reply(
                None,
                None,
                -32700,
                "Message must be a valid JSON object",
            ));
        };
        let Some(id) = members
            .get("id")
            .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        else {
            return Err(error_reply(
                None,
                None,
                -32600,
                "Message must have integer 'id' property",
            ));
        };
        // `null` addresses the root, as no `sessionId` does.
        let session = match members
            .get("sessionId")
            .map(|raw| serde_json::from_str::<Option<Text<'_>>>(raw.get()))
        {
            None | Some(Ok(None)) => None,
            Some(Ok(Some(Text(session)))) => Some(session),
            Some(Err(_)) => {
                return Err(error_reply(
                    Some(id),
                    None,
                    -32600,
                    "Message may have string 'sessionId' property",
                ));
            }
        };
        let root = match session.as_deref() {
            None => {
                let Some(root) = self.roots.get(&client) else {
                    return Err(error_reply(Some(id), None, -32001, "Client is closed."));
                };
                Some(root.as_str())
            }
            Some(session) if self.owners.get(session) == Some(&client) => None,
            Some(session) => {
                return Err(error_reply(
                    Some(id),
                    Some(session),
                    -32001,
                    "Session with given id not found.",
                ));
            }
        };
        let attach = members
            .get("method")
            .and_then(|raw| serde_json::from_str::<Text<'_>>(raw.get()).ok())
            .is_some_and(|Text(method)| attaches(&method));
        let mut out = String::with_capacity(text.len() + 64);
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{{\"id\":{wire}");
        if let Some(root) = root {
            out.push_str(",\"sessionId\":");
            push_json_str(&mut out, root);
        }
        let skip = if root.is_some() {
            &["id", "sessionId"][..]
        } else {
            &["id"][..]
        };
        finish_object(&mut out, &members, skip);
        self.in_flight.insert(wire, InFlight { client, id, attach });
        Ok(out)
    }

    /// Route `text` from Chromium, learning the sessions it creates or ends.
    pub fn inbound(&mut self, text: &str) -> Inbound {
        let Some(head) = Head::parse(text) else {
            return Inbound::Drop;
        };
        if let Some(sent) = head
            .id
            .filter(|_| !self.in_flight.is_empty())
            .and_then(|wire| self.in_flight.remove(&wire))
        {
            return self.reply(&sent, &head, text);
        }
        let Some(session) = head.session.as_deref() else {
            return Inbound::Root;
        };
        let Some(&client) = self.owners.get(session) else {
            return Inbound::Drop;
        };
        match head.method.as_deref() {
            Some("Target.attachedToTarget") => {
                if let Some(child) = inner_session(head.params) {
                    self.owners.insert(child, client);
                }
            }
            Some("Target.detachedFromTarget") => {
                if let Some(child) = inner_session(head.params) {
                    if !self.is_root(client, &child) {
                        self.owners.remove(&child);
                    }
                }
            }
            _ => {}
        }
        Inbound::Deliver {
            client,
            rewritten: self.is_root(client, session).then(|| strip_session(text)),
        }
    }

    /// The reply to `sent`, for its client: with the client's id, and
    /// without the client's root session.
    fn reply(&mut self, sent: &InFlight, head: &Head<'_>, text: &str) -> Inbound {
        if sent.attach {
            if let Some(child) = inner_session(head.result) {
                self.owners.insert(child, sent.client);
            }
        }
        let Ok(members) = serde_json::from_str::<Members<'_>>(text) else {
            return Inbound::Drop;
        };
        let strip = head
            .session
            .as_deref()
            .is_some_and(|session| self.is_root(sent.client, session));
        let mut out = String::with_capacity(text.len() + 16);
        // Writing to a `String` cannot fail.
        let _ = write!(out, "{{\"id\":{}", sent.id);
        finish_object(
            &mut out,
            &members,
            if strip {
                &["id", "sessionId"][..]
            } else {
                &["id"][..]
            },
        );
        Inbound::Deliver {
            client: sent.client,
            rewritten: Some(out),
        }
    }
}

/// A CDP error reply, as Chromium words its own.
pub fn error_reply(id: Option<u64>, session: Option<&str>, code: i32, message: &str) -> String {
    let mut reply = json!({ "error": { "code": code, "message": message } });
    if let Some(id) = id {
        reply["id"] = Value::from(id);
    }
    if let Some(session) = session {
        reply["sessionId"] = Value::from(session);
    }
    reply.to_string()
}

/// `text` as a JSON string literal, appended to `out`.
fn push_json_str(out: &mut String, text: &str) {
    // Serialising a `str` cannot fail.
    out.push_str(&serde_json::to_string(text).unwrap_or_default());
}

/// Append `members` but those named in `skip` to the object `out` has
/// opened (with or without members of its own), and close it. Values go
/// out raw, as they came.
fn finish_object(out: &mut String, members: &Members<'_>, skip: &[&str]) {
    for (key, value) in members {
        let key: &str = key;
        if skip.contains(&key) {
            continue;
        }
        if !out.ends_with('{') {
            out.push(',');
        }
        push_json_str(out, key);
        out.push(':');
        out.push_str(value.get());
    }
    out.push('}');
}

/// `text` (a JSON object) without its top-level `sessionId`; members keep
/// their raw values. Unchanged when it is not an object.
pub fn strip_session(text: &str) -> String {
    let Ok(members) = serde_json::from_str::<Members<'_>>(text) else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(text.len());
    out.push('{');
    finish_object(&mut out, &members, &["sessionId"]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    /// Where `text` goes, and what its client gets.
    fn delivered(routes: &mut Routes, text: &str) -> (ClientId, Value) {
        match routes.inbound(text) {
            Inbound::Deliver { client, rewritten } => {
                (client, parsed(rewritten.as_deref().unwrap_or(text)))
            }
            other => panic!("not delivered: {other:?}"),
        }
    }

    fn two_clients() -> Routes {
        let mut routes = Routes::default();
        routes.add_client(1, "R1".into());
        routes.add_client(2, "R2".into());
        routes
    }

    #[test]
    fn a_root_command_gets_the_client_root_and_its_replies_lose_it() {
        let mut routes = two_clients();
        let out = routes
            .outbound(
                1,
                r#"{"id":7,"method":"Target.getTargets","params":{}}"#,
                100,
            )
            .unwrap();
        assert_eq!(
            parsed(&out),
            json!({ "sessionId": "R1", "id": 100, "method": "Target.getTargets", "params": {} })
        );
        assert_eq!(
            delivered(
                &mut routes,
                r#"{"id":100,"result":{"x":1},"sessionId":"R1"}"#
            ),
            (1, json!({ "id": 7, "result": { "x": 1 } }))
        );
        assert_eq!(
            parsed(&routes.outbound(2, "{}", 101).unwrap_err())["error"]["code"],
            -32600
        );
    }

    #[test]
    fn a_null_session_is_replaced_not_repeated() {
        let mut routes = two_clients();
        let out = routes
            .outbound(1, r#"{"sessionId":null,"id":8,"method":"m"}"#, 102)
            .unwrap();
        assert_eq!(out.matches("sessionId").count(), 1, "{out}");
        assert_eq!(
            parsed(&out),
            json!({ "sessionId": "R1", "id": 102, "method": "m" })
        );
        let refused = parsed(
            &routes
                .outbound(1, r#"{"sessionId":7,"id":9,"method":"m"}"#, 103)
                .unwrap_err(),
        );
        assert_eq!(
            (refused["id"].as_u64(), refused["error"]["code"].as_i64()),
            (Some(9), Some(-32600))
        );
    }

    #[test]
    fn a_reply_without_a_session_reaches_its_sender_with_its_own_id() {
        let mut routes = two_clients();
        routes
            .outbound(2, r#"{"id":1,"sessionId":"R2","method":"m"}"#, 40)
            .unwrap();
        routes.outbound(1, r#"{"id":1,"method":"m"}"#, 41).unwrap();
        let error =
            r#"{"id":40,"error":{"code":-32001,"message":"Session with given id not found."}}"#;
        assert_eq!(
            delivered(&mut routes, error),
            (
                2,
                json!({ "id": 1, "error": { "code": -32001, "message": "Session with given id not found." } })
            )
        );
        assert_eq!(
            delivered(
                &mut routes,
                r#"{"id":41,"error":{"code":-32600,"message":"x"}}"#
            )
            .0,
            1
        );
        assert_eq!(
            routes.inbound(r#"{"id":40,"result":{}}"#),
            Inbound::Root,
            "answered once: a later reply with that id is the broker's"
        );
    }

    #[test]
    fn sessions_a_client_attaches_are_its_own() {
        let mut routes = two_clients();
        routes
            .outbound(1, r#"{"id":1,"method":"Target.attachToTarget","params":{"targetId":"T","flatten":true}}"#, 50)
            .unwrap();
        delivered(
            &mut routes,
            r#"{"id":50,"sessionId":"R1","result":{"sessionId":"C1"}}"#,
        );
        let (client, event) = delivered(
            &mut routes,
            r#"{"sessionId":"R1","method":"Target.attachedToTarget","params":{"sessionId":"C2","targetInfo":{}}}"#,
        );
        assert_eq!(client, 1);
        assert!(event.get("sessionId").is_none());
        for (wire, child) in [(51, "C1"), (52, "C2")] {
            let text = format!(r#"{{"id":2,"sessionId":"{child}","method":"Page.enable"}}"#);
            assert_eq!(
                parsed(&routes.outbound(1, &text, wire).unwrap()),
                json!({ "id": wire, "sessionId": child, "method": "Page.enable" }),
                "own session: kept"
            );
            let reply = format!(r#"{{"id":{wire},"sessionId":"{child}","result":{{}}}}"#);
            assert_eq!(
                delivered(&mut routes, &reply),
                (1, json!({ "id": 2, "sessionId": child, "result": {} }))
            );
            let event = format!(r#"{{"sessionId":"{child}","method":"Page.frameNavigated"}}"#);
            assert_eq!(
                routes.inbound(&event),
                Inbound::Deliver {
                    client: 1,
                    rewritten: None
                },
                "an event passes unchanged"
            );
        }
    }

    #[test]
    fn a_client_cannot_address_another_clients_sessions() {
        let mut routes = two_clients();
        routes
            .outbound(1, r#"{"id":1,"method":"Target.attachToBrowserTarget"}"#, 60)
            .unwrap();
        routes.inbound(r#"{"id":60,"sessionId":"R1","result":{"sessionId":"B1"}}"#);
        for session in ["R1", "B1", "unknown"] {
            let text = format!(r#"{{"id":3,"sessionId":"{session}","method":"Runtime.evaluate"}}"#);
            let refused = parsed(&routes.outbound(2, &text, 61).unwrap_err());
            assert_eq!(refused["error"]["code"], -32001, "{session}");
            assert_eq!(
                (refused["id"].as_u64(), refused["sessionId"].as_str()),
                (Some(3), Some(session))
            );
        }
    }

    #[test]
    fn a_reply_to_a_command_that_is_not_an_attach_grants_nothing() {
        let mut routes = two_clients();
        routes
            .outbound(1, r#"{"id":4,"method":"Target.createTarget"}"#, 70)
            .unwrap();
        routes.inbound(r#"{"id":70,"sessionId":"R1","result":{"sessionId":"X"}}"#);
        assert!(routes
            .outbound(1, r#"{"id":5,"sessionId":"X","method":"a"}"#, 71)
            .is_err());
    }

    #[test]
    fn detach_and_removal_forget_sessions_and_commands() {
        let mut routes = two_clients();
        routes.inbound(
            r#"{"sessionId":"R1","method":"Target.attachedToTarget","params":{"sessionId":"C"}}"#,
        );
        routes.inbound(
            r#"{"sessionId":"R1","method":"Target.detachedFromTarget","params":{"sessionId":"C"}}"#,
        );
        assert_eq!(
            routes.inbound(r#"{"sessionId":"C","method":"x"}"#),
            Inbound::Drop
        );
        routes.inbound(
            r#"{"sessionId":"R2","method":"Target.attachedToTarget","params":{"sessionId":"D"}}"#,
        );
        routes.outbound(2, r#"{"id":1,"method":"m"}"#, 80).unwrap();
        assert_eq!(routes.client_of_root("R2"), Some(2));
        assert_eq!(routes.remove_client(2).as_deref(), Some("R2"));
        assert_eq!(routes.client_of_root("R2"), None);
        assert_eq!(
            routes.inbound(r#"{"sessionId":"D","method":"x"}"#),
            Inbound::Drop
        );
        assert_eq!(
            routes.inbound(r#"{"sessionId":"R2","method":"x"}"#),
            Inbound::Drop
        );
        assert_eq!(
            routes.inbound(r#"{"id":80,"result":{}}"#),
            Inbound::Root,
            "a removed client's command is forgotten"
        );
        routes.outbound(1, r#"{"id":1,"method":"m"}"#, 81).unwrap();
        routes.forget(81);
        assert_eq!(routes.inbound(r#"{"id":81,"result":{}}"#), Inbound::Root);
        assert_eq!(routes.inbound("not json"), Inbound::Drop);
    }

    #[test]
    fn stripping_keeps_every_other_member_raw() {
        let text = r#"{"sessionId":"R","method":"m","params":{"a":[1,2.50,"\u00e9"]},"x\"y":null}"#;
        assert_eq!(
            strip_session(text),
            r#"{"method":"m","params":{"a":[1,2.50,"\u00e9"]},"x\"y":null}"#
        );
        assert_eq!(strip_session("[1]"), "[1]");
    }
}
