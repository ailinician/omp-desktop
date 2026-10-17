use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::fake::{fake_browser, spawn_fake, ATTACH_REFUSED, DETACHED_AT_ONCE, ROOTS};
use super::{lock, Broker, Recv, MAX_COMMAND};
use crate::browser::tests::cdp_result;
use crate::test_support::eventually;

const WAIT: Duration = Duration::from_secs(5);

fn next(client: &super::BrokerClient) -> Value {
    match client.recv(WAIT) {
        Recv::Message(text) => serde_json::from_str(&text).unwrap(),
        other => panic!("no message: {other:?}"),
    }
}

/// The root sessions the fake has attached and not detached.
fn roots(broker: &Broker) -> Value {
    broker.call(ROOTS, json!({}), WAIT).unwrap()["roots"].take()
}

/// The empty success reply to `request`.
fn ok(request: &Value) -> Vec<Value> {
    vec![cdp_result(request, json!({}))]
}

#[test]
fn clients_with_the_same_ids_each_get_their_own_replies() {
    let broker = fake_browser(|request| {
        let echo = request["params"]["n"].clone(); // the reply carries its own copy
        Some(vec![cdp_result(request, json!({ "n": echo }))])
    });
    let (a, b) = (
        broker.connect_browser().unwrap(),
        broker.connect_browser().unwrap(),
    );
    a.send(r#"{"id":1,"method":"Echo","params":{"n":"a"}}"#)
        .unwrap();
    b.send(r#"{"id":1,"method":"Echo","params":{"n":"b"}}"#)
        .unwrap();
    assert_eq!(next(&a), json!({ "id": 1, "result": { "n": "a" } }));
    assert_eq!(next(&b), json!({ "id": 1, "result": { "n": "b" } }));
}

#[test]
fn a_refused_message_is_answered_without_reaching_chromium() {
    let (seen, saw) = mpsc::channel();
    let broker = fake_browser(move |request| {
        let _ = seen.send(request["method"].as_str().map(str::to_owned));
        Some(vec![])
    });
    let client = broker.connect_browser().unwrap();
    client
        .send(r#"{"id":9,"sessionId":"someone-elses","method":"Runtime.evaluate"}"#)
        .unwrap();
    let refused = next(&client);
    assert_eq!(refused["error"]["code"], -32001, "{refused}");
    assert_eq!(refused["id"], 9);
    client.send("not json").unwrap();
    assert_eq!(next(&client)["error"]["code"], -32700);
    client.send(r#"{"id":10,"method":"Reached"}"#).unwrap();
    assert_eq!(
        saw.recv_timeout(WAIT).unwrap().as_deref(),
        Some("Reached"),
        "only the valid one"
    );
}

#[test]
fn a_command_over_the_pipe_limit_is_refused_without_reaching_chromium() {
    let (seen, saw) = mpsc::channel();
    let broker = fake_browser(move |request| {
        let _ = seen.send(request["method"].as_str().map(str::to_owned));
        Some(ok(request))
    });
    let client = broker.connect_browser().unwrap();
    let (head, tail) = (br#"{"id":5,"method":"Big","params":{"pad":""#, br#""}}"#);
    let mut big = vec![b'x'; MAX_COMMAND + 1];
    big[..head.len()].copy_from_slice(head);
    let end = big.len() - tail.len();
    big[end..].copy_from_slice(tail);
    let big = String::from_utf8(big).unwrap();
    client.send(&big).unwrap();
    let refused = next(&client);
    assert_eq!(
        (refused["id"].as_u64(), refused["error"]["code"].as_i64()),
        (Some(5), Some(-32600)),
        "{refused}"
    );
    client.send(r#"{"id":6,"method":"Small"}"#).unwrap();
    assert_eq!(saw.recv_timeout(WAIT).unwrap().as_deref(), Some("Small"));
    assert_eq!(next(&client)["id"], 6);
    assert!(broker.is_alive());
}

#[test]
fn dropping_a_client_detaches_its_session() {
    let broker = fake_browser(|request| Some(ok(request)));
    let client = broker.connect_browser().unwrap();
    assert_eq!(roots(&broker), json!(["root-1"]));
    drop(client);
    // The detach goes out before the next root command does.
    assert_eq!(roots(&broker), json!([]), "the root was detached");
    let again = broker.connect_browser().unwrap();
    assert_eq!(
        roots(&broker),
        json!(["root-2"]),
        "a root name is not reused"
    );
    again.send(r#"{"id":1,"method":"X"}"#).unwrap();
    assert_eq!(next(&again)["id"], 1);
    assert!(broker.call("Browser.getVersion", json!({}), WAIT).is_ok());
}

#[test]
fn a_reply_without_a_session_reaches_its_sender_and_not_a_root_call() {
    let mut held = None;
    let broker = fake_browser(move |request| match request["method"].as_str() {
        Some("Hold") => {
            held = request["id"].as_u64();
            Some(vec![])
        }
        Some("Stale") => {
            // Chromium's answer from its root to a session it no longer
            // knows: no `sessionId`.
            let mut answers = vec![json!({
                "id": request["id"],
                "sessionId": null,
                "error": { "code": -32001, "message": "Session with given id not found." },
            })];
            if let Some(id) = held.take() {
                answers.push(json!({ "id": id, "sessionId": null, "result": { "held": true } }));
            }
            Some(answers)
        }
        _ => Some(ok(request)),
    });
    let client = broker.connect_browser().unwrap();
    let pending = broker.request("Hold", json!({})).unwrap();
    // The client uses the id the broker's own call went out with: had it
    // gone out unchanged, the error would have answered that call.
    let own = pending.id;
    client
        .send(&json!({ "id": own, "method": "Stale" }).to_string())
        .unwrap();
    assert_eq!(
        next(&client),
        json!({
            "id": own,
            "error": { "code": -32001, "message": "Session with given id not found." },
        })
    );
    assert_eq!(pending.wait(WAIT), Some(Ok(json!({ "held": true }))));
}

#[test]
fn a_client_detached_right_behind_its_attach_reply_ends() {
    let broker = fake_browser(|request| Some(ok(request)));
    let page = broker.connect_page(DETACHED_AT_ONCE).unwrap();
    assert_eq!(page.recv(WAIT), Recv::Closed);
    assert!(page.send(r#"{"id":1,"method":"X"}"#).is_err());
    assert!(broker.is_alive(), "only the client ends");
}

#[test]
fn a_refused_attach_adds_no_client() {
    let broker = fake_browser(|request| Some(ok(request)));
    assert!(broker.connect_page(ATTACH_REFUSED).is_err());
    let state = lock(&broker.state);
    assert!(state.outboxes.is_empty());
    assert!(state.attaching.is_empty());
    drop(state);
    assert_eq!(roots(&broker), json!([]));
}

#[test]
fn chromium_going_away_ends_every_client_and_call() {
    let broker = fake_browser(|request| (request["method"] != "Crash").then(Vec::new));
    let client = broker.connect_browser().unwrap();
    client.send(r#"{"id":1,"method":"Crash"}"#).unwrap();
    assert_eq!(client.recv(WAIT), Recv::Closed);
    assert!(!broker.is_alive());
    assert!(client.send(r#"{"id":2,"method":"X"}"#).is_err());
    assert!(broker.call("Browser.getVersion", json!({}), WAIT).is_err());
    assert!(broker.connect_browser().is_err());
}

#[test]
fn a_page_client_ends_when_its_page_session_is_detached() {
    let broker = fake_browser(|request| {
        if request["method"] == "Page.close" {
            // Chromium tells the root session that the page session ended.
            return Some(vec![json!({
                "sessionId": null,
                "method": "Target.detachedFromTarget",
                "params": { "sessionId": "root-1", "targetId": "T" },
            })]);
        }
        Some(ok(request))
    });
    let page = broker.connect_page("T").unwrap();
    page.send(r#"{"id":1,"method":"Page.close"}"#).unwrap();
    assert_eq!(page.recv(WAIT), Recv::Closed);
    assert!(broker.is_alive(), "only the client ends");
}

#[test]
fn a_broker_nobody_holds_ends_and_so_does_its_fake() {
    let (broker, fake) = spawn_fake(|request| Some(ok(request)));
    let client = broker.connect_browser().unwrap();
    drop(broker);
    client.send(r#"{"id":1,"method":"X"}"#).unwrap();
    assert_eq!(next(&client)["id"], 1, "a client keeps the broker");
    assert!(!fake.is_finished());
    drop(client);
    eventually(WAIT, || fake.is_finished().then_some(())).expect("the fake saw its pipe end");
}

#[test]
fn close_wakes_a_blocked_reader_and_is_idempotent() {
    let (_keep_writer, from_chromium) = {
        let (r, w) = std::io::pipe().unwrap();
        (w, r)
    };
    let (_keep_reader, to_chromium) = std::io::pipe().unwrap();
    let broker = Broker::start(from_chromium, to_chromium).unwrap();
    std::thread::sleep(Duration::from_millis(50)); // the reader blocks
    let reader_finished = || {
        lock(&broker.threads)
            .as_ref()
            .is_some_and(|threads| threads.reader.is_finished())
    };
    assert!(!reader_finished(), "nothing to read: the reader waits");
    let closing = Instant::now();
    broker.close();
    broker.close();
    assert!(!broker.is_alive());
    // Unix: nothing to cancel, the pipe stays open; the reader leaves once
    // it next reads. Windows: the blocked read is cancelled.
    if cfg!(windows) {
        let left = Duration::from_secs(1).saturating_sub(closing.elapsed());
        eventually(left, || reader_finished().then_some(())).expect("reader woken within a second");
    }
}
