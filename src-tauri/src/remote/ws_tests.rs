use super::*;
use crate::remote::ws::is_idle;
use base64::Engine;
use std::time::Duration;
use tungstenite::protocol::Role;
use tungstenite::Message;

pub(super) trait Rw: Read + Write {}
impl<T: Read + Write> Rw for T {}

struct Client {
    ws: tungstenite::WebSocket<Box<dyn Rw>>,
}

fn tab(id: &str) -> TabMeta {
    TabMeta {
        id: id.to_owned(),
        title: format!("Tab {id}"),
        project: "proj".to_owned(),
    }
}

fn payload(ids: &[&str]) -> PublishPayload {
    PublishPayload {
        tabs: ids.iter().map(|id| tab(id)).collect(),
        view: None,
        theme: None,
    }
}

impl TestServer {
    fn tcp(&self) -> TcpStream {
        self.tcp_with(Duration::from_millis(100))
    }

    /// Completed upgrade with a same-origin request, not yet authenticated.
    fn ws_connect(&self) -> Client {
        let mut stream = self.connect(Duration::from_millis(100));
        let scheme = if self.tls { "https" } else { "http" };
        let request = format!(
            "GET /ws HTTP/1.1\r\nHost: {host}\r\nOrigin: {scheme}://{host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            host = self.host()
        );
        stream.write_all(request.as_bytes()).unwrap();
        // Byte-wise, so no frame bytes are swallowed with the head.
        let mut head = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !head.ends_with(b"\r\n\r\n") {
            assert!(Instant::now() < deadline, "no upgrade response");
            let mut byte = [0_u8];
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                Ok(_) => panic!("closed during handshake"),
                Err(error) if is_idle(&error) => {}
                Err(error) => panic!("{error}"),
            }
        }
        assert!(
            head.starts_with(b"HTTP/1.1 101"),
            "{}",
            String::from_utf8_lossy(&head)
        );
        Client {
            ws: tungstenite::WebSocket::from_raw_socket(stream, Role::Client, None),
        }
    }

    fn pair_device(&self) -> String {
        self.pair_named("Phone")
    }

    fn pair_named(&self, name: &str) -> String {
        let code = self.pairing.start(Instant::now()).unwrap();
        let reply = self.post_pair(&format!(r#"{{"code":"{code}","name":"{name}"}}"#));
        assert_eq!(reply.status, 200);
        let body: Value = serde_json::from_slice(&reply.body).unwrap();
        body["token"].as_str().unwrap().to_owned()
    }

    fn authed_with(&self, token: &str) -> Client {
        let mut client = self.ws_connect();
        client.send_text(&format!(r#"{{"t":"auth","token":"{token}"}}"#));
        assert_eq!(client.read_json()["t"], "hello");
        assert_eq!(client.read_json()["t"], "tabs");
        client
    }

    fn authed_socket(&self) -> Client {
        let token = self.pair_device();
        self.authed_with(&token)
    }
}

impl Client {
    fn send_text(&mut self, text: &str) {
        self.ws.send(Message::text(text)).unwrap();
    }

    /// Next message, `None` when `within` passes or the connection ended.
    fn read_message(&mut self, within: Duration) -> Option<Message> {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.ws.read() {
                Ok(message) => return Some(message),
                Err(tungstenite::Error::Io(error)) if is_idle(&error) => {}
                Err(_) => return None,
            }
        }
        None
    }

    fn read_json_where_within(&mut self, pick: impl Fn(&Value) -> bool, within: Duration) -> Value {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(Message::Text(text)) = self.read_message(left) else {
                panic!("no matching message");
            };
            let value: Value = serde_json::from_str(text.as_str()).unwrap();
            if pick(&value) {
                return value;
            }
        }
    }

    fn read_json_where(&mut self, pick: impl Fn(&Value) -> bool) -> Value {
        self.read_json_where_within(pick, Duration::from_secs(5))
    }

    fn read_json(&mut self) -> Value {
        self.read_json_where(|_| true)
    }

    /// Close code of the next close frame; text frames before it fail the
    /// test. `None`: the connection ended without a close frame.
    fn read_close_code_within(&mut self, within: Duration) -> Option<u16> {
        match self.read_message(within)? {
            Message::Close(frame) => frame.map(|frame| u16::from(frame.code)),
            other => panic!("expected close, got {other:?}"),
        }
    }

    fn read_close_code(&mut self) -> Option<u16> {
        self.read_close_code_within(Duration::from_secs(5))
    }
}

#[test]
fn unauthenticated_socket_gets_4401_and_no_tabs() {
    let s = test_server("ws-unauth");
    s.hub.publish(payload(&["t1"]));
    let mut ws = s.ws_connect();
    ws.send_text(r#"{"t":"auth","token":"nope"}"#);
    assert_eq!(ws.read_close_code(), Some(4401));
    let mut anything = s.ws_connect();
    anything.send_text(r#"{"t":"select","tabId":"t1","reqId":"x"}"#);
    assert_eq!(
        anything.read_close_code(),
        Some(4401),
        "no auth, no actions"
    );
    let mut silent = s.ws_connect(); // never authenticates
    assert_eq!(
        silent.read_close_code_within(Duration::from_secs(7)),
        Some(4401)
    );
    let token = s.pair_device();
    let mut ok = s.ws_connect();
    ok.send_text(&format!(r#"{{"t":"auth","token":"{token}"}}"#));
    assert_eq!(ok.read_json()["t"], "hello");
    let tabs = ok.read_json();
    assert_eq!(tabs["t"], "tabs");
    assert_eq!(tabs["tabs"][0]["id"], "t1");
    assert_eq!(tabs["tabs"][0]["busy"], false);
    assert_eq!(tabs["tabs"][0]["hasAsk"], false);
}

#[test]
fn action_round_trip_and_timeout() {
    let s = test_server("ws-action");
    let mut ws = s.authed_socket();
    ws.send_text(r#"{"t":"action","reqId":"r1","tabId":"t1","kind":"prompt","text":"hi"}"#);
    let a = s.emit.next_action();
    assert_eq!((a.kind.as_str(), a.text.as_deref()), ("prompt", Some("hi")));
    assert_eq!(a.tab_id.as_str(), "t1");
    assert_ne!(a.req_id.as_str(), "r1", "hub uses its own correlation id");
    s.hub.action_result(&a.req_id, true, None);
    let ack = ws.read_json_where(|m| m["t"] == "ack");
    assert_eq!(
        (ack["reqId"].as_str(), ack["ok"].as_bool()),
        (Some("r1"), Some(true))
    );
    ws.send_text(r#"{"t":"select","reqId":"r3","tabId":"t2"}"#);
    let select = s.emit.next_action();
    assert_eq!(select.kind, "select");
    s.hub
        .action_result(&select.req_id, false, Some("no such tab"));
    let ack = ws.read_json_where(|m| m["t"] == "ack");
    assert_eq!(
        (ack["reqId"].as_str(), ack["error"].as_str()),
        (Some("r3"), Some("no such tab"))
    );
    ws.send_text(r#"{"t":"action","reqId":"r2","tabId":"t1","kind":"abort"}"#);
    let abort = s.emit.next_action();
    let ack = ws.read_json_where_within(|m| m["t"] == "ack", Duration::from_secs(17));
    assert_eq!(
        (ack["reqId"].as_str(), ack["ok"].as_bool()),
        (Some("r2"), Some(false))
    );
    assert_eq!(ack["error"], "desktop did not answer");
    s.hub.action_result(&abort.req_id, true, None); // late answer is ignored
}

#[test]
fn same_req_id_from_two_connections_is_forwarded_independently_and_duplicate_on_one_is_refused() {
    let s = test_server("ws-reqid-collision");
    let mut first = s.authed_socket();
    let mut second = s.authed_socket();
    first.send_text(r#"{"t":"action","reqId":"same","tabId":"t1","kind":"abort"}"#);
    second.send_text(r#"{"t":"action","reqId":"same","tabId":"t2","kind":"abort"}"#);
    let a1 = s.emit.next_action();
    let a2 = s.emit.next_action();
    assert_ne!(a1.req_id, a2.req_id);
    // A duplicate reqId on the same connection is still refused.
    first.send_text(r#"{"t":"action","reqId":"same","tabId":"t1","kind":"abort"}"#);
    let dup = first.read_json_where(|m| m["t"] == "ack");
    assert_eq!(
        (
            dup["reqId"].as_str(),
            dup["ok"].as_bool(),
            dup["error"].as_str()
        ),
        (Some("same"), Some(false), Some("duplicate reqId"))
    );
    s.hub.action_result(&a1.req_id, true, None);
    s.hub
        .action_result(&a2.req_id, false, Some("tab not available"));
    let ack1 = first.read_json_where(|m| m["t"] == "ack");
    let ack2 = second.read_json_where(|m| m["t"] == "ack");
    assert_eq!(
        (ack1["reqId"].as_str(), ack1["ok"].as_bool()),
        (Some("same"), Some(true))
    );
    assert_eq!(
        (
            ack2["reqId"].as_str(),
            ack2["ok"].as_bool(),
            ack2["error"].as_str()
        ),
        (Some("same"), Some(false), Some("tab not available"))
    );
}

#[test]
fn bad_actions_are_refused_without_reaching_the_webview() {
    let s = test_server("ws-bad-action");
    let mut ws = s.authed_socket();
    ws.send_text(r#"{"t":"action","reqId":"b1","tabId":"t1","kind":"rm-rf"}"#);
    assert_eq!(
        ws.read_json_where(|m| m["t"] == "ack")["error"],
        "bad request"
    );
    let big = "x".repeat(32 * 1024 + 1);
    ws.send_text(&format!(
        r#"{{"t":"action","reqId":"b2","tabId":"t1","kind":"prompt","text":"{big}"}}"#
    ));
    assert_eq!(
        ws.read_json_where(|m| m["t"] == "ack")["error"],
        "bad request"
    );
    let value = "x".repeat(32 * 1024 + 1);
    ws.send_text(&format!(
        r#"{{"t":"action","reqId":"b4","tabId":"t1","kind":"answer","askId":"a","value":"{value}"}}"#
    ));
    assert_eq!(
        ws.read_json_where(|m| m["t"] == "ack")["error"],
        "bad request"
    );
    // No reqId, no way to answer: dropped without reaching the webview.
    ws.send_text(r#"{"t":"action","tabId":"t1","kind":"abort"}"#);
    ws.send_text(r#"{"t":"action","reqId":"","tabId":"t1","kind":"abort"}"#);
    ws.send_text(r#"{"t":"action","reqId":"b3","tabId":"t1","kind":"abort"}"#);
    assert_eq!(
        s.emit.next_action().tab_id,
        "t1",
        "nothing else was forwarded"
    );
}

#[test]
fn revoke_disconnects_and_pairing_is_rate_limited() {
    let s = test_server("ws-revoke");
    let token = s.pair_device();
    let mut open = s.authed_with(&token);
    let device = s.devices.list().remove(0);
    assert_eq!(s.hub.connected_devices(), vec![device.id.clone()]);
    assert!(s.hub.revoke(&device.id));
    assert!(matches!(open.read_close_code(), None | Some(4401)));
    let mut again = s.ws_connect();
    again.send_text(&format!(r#"{{"t":"auth","token":"{token}"}}"#));
    assert_eq!(again.read_close_code(), Some(4401), "revoked token is dead");
    for _ in 0..5 {
        assert_eq!(s.post_pair(r#"{"code":"bad","name":"x"}"#).status, 403);
    }
    assert_eq!(s.post_pair(r#"{"code":"bad","name":"x"}"#).status, 429);
}

#[test]
fn oversized_frame_and_too_many_connections_are_rejected() {
    let s = test_server("ws-limits");
    let token = s.pair_device();
    let mut sockets: Vec<Client> = (0..4).map(|_| s.authed_with(&token)).collect();
    let mut fifth = s.ws_connect();
    fifth.send_text(&format!(r#"{{"t":"auth","token":"{token}"}}"#));
    assert_eq!(fifth.read_close_code(), Some(4429));
    // Four more from a second device fill the hub; the ninth is refused.
    let other = s.pair_named("Tablet");
    sockets.extend((0..4).map(|_| s.authed_with(&other)));
    let third = s.pair_named("Laptop");
    let mut ninth = s.ws_connect();
    ninth.send_text(&format!(r#"{{"t":"auth","token":"{third}"}}"#));
    assert_eq!(ninth.read_close_code(), Some(4429));
    // 1 MiB + 1 text frame.
    let mut big = sockets.remove(0);
    // The server may cut TCP as soon as it sees the oversized frame header,
    // before the client finishes writing its payload.
    let _ = big.ws.send(Message::text("x".repeat((1 << 20) + 1)));
    let closed = big.read_message(Duration::from_secs(5));
    assert!(
        matches!(closed, None | Some(Message::Close(_))),
        "oversized frame ends the connection: {closed:?}"
    );
    // The others are unaffected.
    s.hub.publish(payload(&["t9"]));
    assert_eq!(
        sockets[0].read_json_where(|m| m["t"] == "tabs")["tabs"][0]["id"],
        "t9"
    );
}

#[test]
fn idle_connections_cannot_exhaust_the_server() {
    let s = test_server("ws-flood");
    let idle: Vec<TcpStream> = (0..16).map(|_| s.tcp()).collect();
    std::thread::sleep(Duration::from_millis(500));
    let mut extra = s.tcp();
    let mut byte = [0_u8];
    let deadline = Instant::now() + Duration::from_secs(3);
    // Dropped: end of stream or a reset, never a wait for a request.
    while matches!(extra.read(&mut byte), Err(ref e) if is_idle(e)) {
        assert!(Instant::now() < deadline, "17th connection was not dropped");
    }
    drop(idle);
}

#[test]
fn closes_4503_while_the_webview_is_not_ready() {
    let s = test_server("ws-ready");
    let mut live = s.authed_socket();
    s.hub.set_ready(false);
    assert!(
        matches!(live.read_close_code(), None | Some(4503)),
        "open sockets are closed immediately, even without a close frame"
    );
    let mut fresh = s.ws_connect();
    assert_eq!(
        fresh.read_close_code(),
        Some(4503),
        "new sockets are refused"
    );
    s.hub.set_ready(true);
    let _ = s.authed_socket();
}

#[test]
fn push_subscriptions_are_stored_capped_and_acked() {
    let s = test_server("ws-push");
    let mut ws = s.authed_socket();
    let key =
        |bytes: usize| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(vec![1_u8; bytes]);
    let (p256dh, auth) = (key(65), key(16));
    let subscribe = |ws: &mut Client, endpoint: &str| {
        ws.send_text(&format!(
            r#"{{"t":"push-subscribe","sub":{{"endpoint":"{endpoint}","keys":{{"p256dh":"{p256dh}","auth":"{auth}"}}}}}}"#
        ));
        ws.read_json_where(|m| m["t"] == "ack")
    };
    for n in 0..4 {
        let ack = subscribe(&mut ws, &format!("https://fcm.googleapis.com/fcm/send/{n}"));
        assert_eq!(
            (ack["kind"].as_str(), ack["ok"].as_bool()),
            (Some("push"), Some(true))
        );
        assert!(ack["reqId"].is_null());
    }
    let ack = subscribe(&mut ws, "https://fcm.googleapis.com/fcm/send/5");
    assert_eq!(
        (ack["ok"].as_bool(), ack["error"].as_str()),
        (Some(false), Some("too many push subscriptions"))
    );
    let ack = subscribe(&mut ws, "https://evil.example/x");
    assert_eq!(ack["error"], "bad subscription");
    assert_eq!(s.devices.list()[0].push.len(), 4);
    ws.send_text(r#"{"t":"push-unsubscribe"}"#);
    assert_eq!(ws.read_json_where(|m| m["t"] == "ack")["ok"], true);
    assert!(s.devices.list()[0].push.is_empty());
}

/// Bytes trickled to a socket that never authenticates: the server must cut
/// it at the auth deadline (plus the close grace), not keep reading while
/// bytes keep coming. The write failing is how the client sees that.
fn trickle_frame_is_cut(client: &mut Client, delay: Duration) -> bool {
    let start = Instant::now();
    // A masked 4000-byte text frame: the socket is never idle long enough for
    // a read timeout to end a poll.
    let mut bytes = vec![0x81, 0xFE, 0x0F, 0xA0];
    bytes.extend(std::iter::repeat_n(0_u8, 4004));
    for byte in bytes {
        if start.elapsed() > Duration::from_millis(7500) {
            return false;
        }
        let raw = client.ws.get_mut();
        if raw.write_all(&[byte]).and_then(|()| raw.flush()).is_err() {
            return true;
        }
        std::thread::sleep(delay);
    }
    false
}

fn trickle_is_cut(s: &TestServer) -> bool {
    let mut client = s.ws_connect();
    trickle_frame_is_cut(&mut client, Duration::from_millis(10))
}

#[test]
fn trickling_bytes_do_not_keep_an_unauthenticated_tls_socket_open() {
    assert!(trickle_is_cut(&test_server("ws-trickle-tls")));
}

#[test]
fn trickling_bytes_do_not_keep_an_unauthenticated_plain_socket_open() {
    assert!(trickle_is_cut(&test_server_with(false, "ws-trickle-plain")));
}

#[test]
fn connecting_and_disconnecting_phones_are_announced() {
    let s = test_server("ws-conn-events");
    let token = s.pair_device();
    let device = s.devices.list().remove(0).id;
    let wait_for = |expected: Vec<String>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.emit.connection_log().last() != Some(&expected) {
            assert!(Instant::now() < deadline, "no announcement of {expected:?}");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let ws = s.authed_with(&token);
    wait_for(vec![device]);
    drop(ws);
    wait_for(Vec::new());
}

#[test]
fn revoked_connection_gets_its_close_and_nothing_else() {
    let s = test_server("ws-late-action");
    let _token = s.pair_device();
    let device = s.devices.list().remove(0);
    let (tx, rx) = std::sync::mpsc::sync_channel(8);
    let conn = s
        .hub
        .register(&device, tx, None, s.hub.generation())
        .unwrap();
    assert!(s.hub.revoke(&device.id));
    let action = Action {
        req_id: "late".to_owned(),
        device_id: device.id,
        tab_id: "t1".to_owned(),
        kind: "abort".to_owned(),
        text: None,
        ask_id: None,
        value: None,
    };
    s.hub.action(conn, &action);
    assert!(
        s.emit.try_action(Duration::from_millis(300)).is_none(),
        "late action of a revoked socket is not forwarded"
    );
    let mut got = Vec::new();
    while let Ok(out) = rx.try_recv() {
        got.push(matches!(out, crate::remote::hub::Out::Close(4401)));
    }
    assert_eq!(got, [false, false, true], "hello, tabs, then the close");
    assert_eq!(s.hub.connected_devices(), Vec::<String>::new());
}

#[test]
fn upgraded_sockets_hold_their_connection_slot() {
    let s = test_server("ws-slot");
    let token = s.pair_device();
    let mut held: Vec<Client> = (0..4).map(|_| s.authed_with(&token)).collect();
    held.extend((4..MAX_IN_FLIGHT_PER_IP).map(|_| s.ws_connect()));
    let mut extra = s.tcp();
    let mut byte = [0_u8];
    let deadline = Instant::now() + Duration::from_secs(3);
    while matches!(extra.read(&mut byte), Err(ref e) if is_idle(e)) {
        assert!(
            Instant::now() < deadline,
            "connection over the per-peer cap was not dropped"
        );
    }
    drop(held);
}

#[test]
fn stalled_peers_trickling_a_frame_lose_their_sockets_not_just_their_registration() {
    let s = test_server_with(false, "ws-stalled");
    let tokens = [s.pair_named("A"), s.pair_named("B")];
    let mut clients: Vec<Client> = [0, 0, 0, 1, 1]
        .iter()
        .map(|device| s.authed_with(&tokens[*device]))
        .collect();
    // Each peer starts a half-megabyte frame and never reads what it is sent.
    let mut head = vec![0x81, 0x80 | 127];
    head.extend_from_slice(&500_000_u64.to_be_bytes());
    head.extend_from_slice(&[0; 4]);
    for client in &mut clients {
        client.ws.get_mut().write_all(&head).unwrap();
    }
    let padding = "x".repeat(50_000);
    for n in 0..400 {
        s.hub.publish(PublishPayload {
            tabs: vec![TabMeta {
                id: "t1".to_owned(),
                title: format!("{n}{padding}"),
                project: "proj".to_owned(),
            }],
            view: None,
            theme: None,
        });
        for client in &mut clients {
            let _ = client.ws.get_mut().write_all(&[0]); // keeps the read busy
        }
    }
    assert!(
        s.hub.connected_devices().is_empty(),
        "overflowing outboxes are dropped from the hub"
    );
    // The sockets themselves must be gone: a write to a closed one fails.
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut open: Vec<Client> = clients;
    while !open.is_empty() {
        assert!(
            Instant::now() < deadline,
            "{} sockets still open",
            open.len()
        );
        open.retain_mut(|client| client.ws.get_mut().write_all(&[0]).is_ok());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn revoking_an_authenticated_socket_cuts_tcp_immediately() {
    let s = test_server_with(false, "ws-revoke-physical");
    let mut client = s.authed_socket();
    // Keep the reader inside an unfinished frame rather than its outbox loop.
    client
        .ws
        .get_mut()
        .write_all(&[0x81, 0xfe, 0x0f, 0xa0, 0, 0, 0, 0])
        .unwrap();
    let device = s.devices.list().remove(0);
    assert!(s.hub.revoke(&device.id));
    assert!(
        crate::test_support::wait_until(Duration::from_secs(1), || {
            client.ws.get_mut().write_all(&[0]).is_err()
        }),
        "revocation removed the registration but left TCP open"
    );
}

fn authenticated_trickle_is_cut(tls: bool, delay: Duration, name: &str) {
    let s = test_server_with(tls, name);
    let mut client = s.authed_socket();
    assert!(
        trickle_frame_is_cut(&mut client, delay),
        "trickled frame was not cut"
    );
    assert_eq!(s.hub.connected_devices(), Vec::<String>::new());
}

#[test]
fn authenticated_continuous_trickle_has_an_absolute_deadline() {
    authenticated_trickle_is_cut(false, Duration::from_millis(10), "ws-authed-trickle");
}

#[test]
fn authenticated_tls_trickle_has_an_absolute_deadline() {
    authenticated_trickle_is_cut(true, Duration::from_millis(10), "ws-authed-trickle-tls");
}

#[test]
fn authenticated_partial_reads_do_not_reset_the_frame_deadline() {
    authenticated_trickle_is_cut(false, Duration::from_millis(100), "ws-authed-partials");
}
