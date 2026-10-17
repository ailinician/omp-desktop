//! A Chromium stand-in on the far end of an in-memory pipe pair, for the
//! suites that drive a [`Broker`] (and what sits on it) without a browser.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::thread::JoinHandle;

use serde_json::{json, Value};

use super::Broker;

/// A root command of the fixture's own: its result's `roots` lists the root
/// sessions attached and not detached since, in order.
pub const ROOTS: &str = "Fixture.roots";

/// A target id whose attach fails, as for a target that is gone.
pub const ATTACH_REFUSED: &str = "Fixture.refusedTarget";

/// A target id whose attach succeeds, and whose new session is detached
/// right behind the reply, as when the page closes at that moment.
pub const DETACHED_AT_ONCE: &str = "Fixture.detachedTarget";

/// A broker whose "Chromium" answers attaching, detaching,
/// `Browser.getVersion` and [`ROOTS`] on the root session itself and hands
/// every other command to `reply`: a client's root-session command without
/// its `sessionId` (as the client wrote it, with the id the broker gave it),
/// a command on a session the client attached with it, a broker root
/// command (discovery) as sent. `None` ends the pipe, as a crash would;
/// `Some(messages)` sends them (none = no answer). A message without
/// `sessionId` goes back on the session the command came on;
/// `"sessionId": null` sends it on the root session. Root sessions are
/// named `root-1`, `root-2`, … in attach order, and never reused.
pub fn fake_browser(
    reply: impl FnMut(&Value) -> Option<Vec<Value>> + Send + 'static,
) -> Arc<Broker> {
    spawn_fake(reply).0
}

/// [`fake_browser`], and the fake's thread: it ends when the pipe does —
/// once the broker is closed or dropped, or `reply` gives `None`.
pub fn spawn_fake(
    mut reply: impl FnMut(&Value) -> Option<Vec<Value>> + Send + 'static,
) -> (Arc<Broker>, JoinHandle<()>) {
    let (to_broker, from_fake) = {
        let (r, w) = std::io::pipe().expect("pipe");
        (w, r)
    };
    let (from_broker, to_fake) = std::io::pipe().expect("pipe");
    let broker = Broker::start(from_fake, to_fake).expect("broker");
    let fake = std::thread::spawn(move || {
        let mut to_broker = to_broker;
        let mut roots = Roots::default();
        let mut reader = BufReader::new(from_broker);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            if !matches!(reader.read_until(0, &mut buf), Ok(n) if n > 0) {
                return;
            }
            buf.pop();
            let Ok(mut request) = serde_json::from_slice::<Value>(&buf) else {
                return;
            };
            let session = request["sessionId"].as_str().map(str::to_owned);
            let answers = match session.as_deref() {
                None => roots.answer(&request).or_else(|| reply(&request)),
                Some(s) if roots.live.contains(s) => {
                    if let Some(fields) = request.as_object_mut() {
                        fields.remove("sessionId");
                    }
                    reply(&request)
                }
                Some(_) => reply(&request),
            };
            let Some(answers) = answers else {
                return; // dropping the write end ends the pipe
            };
            for mut answer in answers {
                match answer.get("sessionId") {
                    Some(Value::Null) => {
                        if let Some(fields) = answer.as_object_mut() {
                            fields.remove("sessionId");
                        }
                    }
                    Some(_) => {}
                    None => {
                        if let Some(s) = &session {
                            answer["sessionId"] = Value::from(s.as_str());
                        }
                    }
                }
                let mut bytes = answer.to_string().into_bytes();
                bytes.push(0);
                if to_broker.write_all(&bytes).is_err() {
                    return;
                }
            }
        }
    });
    (broker, fake)
}

/// The root sessions the fake hands out.
#[derive(Default)]
struct Roots {
    live: BTreeSet<String>,
    /// How many were attached so far: the next is named after it.
    attached: u64,
}

impl Roots {
    /// The answer to a root-session `request` the fake handles itself;
    /// `None` for any other.
    fn answer(&mut self, request: &Value) -> Option<Vec<Value>> {
        let answer = match request["method"].as_str()? {
            "Target.attachToBrowserTarget" | "Target.attachToTarget" => {
                return Some(self.attach(request));
            }
            "Target.detachFromTarget" => {
                if let Some(root) = request["params"]["sessionId"].as_str() {
                    self.live.remove(root);
                }
                json!({ "id": request["id"], "result": {} })
            }
            ROOTS => json!({ "id": request["id"], "result": { "roots": self.live } }),
            "Browser.getVersion" => json!({
                "id": request["id"],
                "result": {
                    "protocolVersion": "1.3",
                    "product": "FakeChrome/1.0",
                    "revision": "@0",
                    "userAgent": "FakeChrome",
                    "jsVersion": "0.0",
                },
            }),
            _ => return None,
        };
        Some(vec![answer])
    }

    fn attach(&mut self, request: &Value) -> Vec<Value> {
        let target = request["params"]["targetId"].as_str();
        if target == Some(ATTACH_REFUSED) {
            return vec![json!({
                "id": request["id"],
                "error": { "code": -32602, "message": "No target with given id found" },
            })];
        }
        self.attached += 1;
        let root = format!("root-{}", self.attached);
        let mut answers = vec![json!({ "id": request["id"], "result": { "sessionId": root } })];
        if target == Some(DETACHED_AT_ONCE) {
            answers.push(json!({
                "method": "Target.detachedFromTarget",
                "params": { "sessionId": root, "targetId": DETACHED_AT_ONCE },
            }));
        } else {
            self.live.insert(root);
        }
        answers
    }
}
