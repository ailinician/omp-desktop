//! Proxy tests: admission before the lazy launch, the connection cap,
//! `/json` rewriting, the splice, shutdown, and what the tap reports through
//! it. Also the WebSocket frame builders and the report recorder the other
//! proxy modules' tests share.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::http::{rewrite_json, MAX_HEAD};
use super::{AgentTarget, Ensure, Proxy, MAX_CONNECTIONS};
use crate::browser::lock;
use crate::browser::tests::{eventually, http_get};

const MASK: [u8; 4] = [0x11, 0x22, 0x33, 0x44];

/// One frame as a client (`masked`) or a server sends it, with the shortest
/// length encoding.
pub(super) fn frame(opcode: u8, fin: bool, masked: bool, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
    let flag = if masked { 0x80 } else { 0 };
    match payload.len() {
        n @ 0..=125 => out.push(flag | u8::try_from(n).unwrap()),
        n @ 126..=0xFFFF => {
            out.push(flag | 0x7E);
            out.extend_from_slice(&u16::try_from(n).unwrap().to_be_bytes());
        }
        n => {
            out.push(flag | 0x7F);
            out.extend_from_slice(&u64::try_from(n).unwrap().to_be_bytes());
        }
    }
    if masked {
        out.extend_from_slice(&MASK);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
    } else {
        out.extend_from_slice(payload);
    }
    out
}

/// A client's text message in one frame.
pub(super) fn masked_text(text: &str) -> Vec<u8> {
    frame(0x1, true, true, text.as_bytes())
}

/// A server's text message in one frame.
pub(super) fn text(text: &str) -> Vec<u8> {
    frame(0x1, true, false, text.as_bytes())
}

/// The targets a tap reported.
#[derive(Clone, Default)]
pub(super) struct Reported(Arc<Mutex<Vec<String>>>);

impl Reported {
    pub(super) fn callback(&self) -> AgentTarget {
        let seen = Arc::clone(&self.0);
        Arc::new(move |target| lock(&seen).push(target.to_owned()))
    }

    pub(super) fn all(&self) -> Vec<String> {
        lock(&self.0).clone()
    }
}

/// A proxy that nobody listens to the agent's pages of.
fn nobody() -> AgentTarget {
    Arc::new(|_| {})
}

/// An `Ensure` that counts its calls and refuses to launch.
fn refusing_ensure() -> (Ensure, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&calls);
    let ensure: Ensure = Arc::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Err("must not launch".into())
    });
    (ensure, calls)
}

/// The head of an agent's WebSocket upgrade to `path`.
fn upgrade(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\r\n")
}

/// Serves `/json/version` and, for any other request, echoes whatever the
/// client sends after the request head.
fn fake_upstream() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            thread::spawn(move || serve_fake(&stream, port));
        }
    });
    port
}

fn serve_fake(stream: &TcpStream, port: u16) {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    skip_head(&mut reader);
    let mut out = stream;
    if request_line.starts_with("GET /json/version") {
        let body =
            format!(r#"{{"webSocketDebuggerUrl":"ws://127.0.0.1:{port}/devtools/browser/x"}}"#);
        write!(
            out,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    } else {
        let _ = io::copy(&mut reader, &mut out);
    }
}

/// Reads the rest of a request head, up to its blank line.
fn skip_head(reader: &mut impl BufRead) {
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
}

/// Sends `request`, then collects the reply until EOF or a quiet second.
fn raw_roundtrip(port: u16, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut got = Vec::new();
    let mut chunk = [0u8; 256];
    while let Ok(n @ 1..) = stream.read(&mut chunk) {
        got.extend_from_slice(&chunk[..n]);
    }
    got
}

/// Reads from `stream` until what arrived ends with `tail`.
fn read_until_ends_with(stream: &mut TcpStream, tail: &[u8]) -> Vec<u8> {
    let mut got = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    while !got.ends_with(tail) {
        let n = stream.read(&mut chunk).unwrap();
        assert!(n > 0, "closed before the end: {} bytes", got.len());
        got.extend_from_slice(&chunk[..n]);
    }
    got
}

#[test]
fn json_bodies_point_at_the_proxy() {
    assert_eq!(
        rewrite_json(
            r#"{"a":"ws://127.0.0.1:9222/x","b":"ws://localhost:9222/y"}"#,
            9222,
            4000
        ),
        r#"{"a":"ws://127.0.0.1:4000/x","b":"ws://127.0.0.1:4000/y"}"#
    );
}

#[test]
fn version_is_rewritten_and_raw_traffic_is_spliced() {
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    let body = http_get(proxy.port(), "/json/version").unwrap();
    assert!(
        body.contains(&format!("127.0.0.1:{}", proxy.port())),
        "{body}"
    );
    // `PING` rides in the same write as the head: it must reach upstream.
    let request = format!("{}PING", upgrade("/devtools/browser/x"));
    let echoed = raw_roundtrip(proxy.port(), request.as_bytes());
    assert!(echoed.ends_with(b"PING"), "{echoed:?}");
}

#[test]
fn concurrent_first_connections_launch_once() {
    let up = fake_upstream();
    let calls = Arc::new(AtomicUsize::new(0));
    let launched = Arc::new(Mutex::new(None::<u16>));
    let c = Arc::clone(&calls);
    let l = Arc::clone(&launched);
    let ensure: Ensure = Arc::new(move || {
        let mut g = lock(&l);
        if g.is_none() {
            c.fetch_add(1, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(200));
            *g = Some(up);
        }
        Ok(g.unwrap())
    });
    let proxy = Proxy::bind(ensure, nobody()).unwrap();
    let handles: Vec<_> = (0..5)
        .map(|_| {
            let p = proxy.port();
            thread::spawn(move || http_get(p, "/json/version"))
        })
        .collect();
    for h in handles {
        assert!(h.join().unwrap().is_ok());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Refused requests get a 403 and never launch Chromium.
fn assert_forbidden(request: &[u8]) {
    let (ensure, calls) = refusing_ensure();
    let proxy = Proxy::bind(ensure, nobody()).unwrap();
    let response = raw_roundtrip(proxy.port(), request);
    assert!(
        response.starts_with(b"HTTP/1.1 403 Forbidden\r\n"),
        "{}: {response:?}",
        String::from_utf8_lossy(request)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no lazy launch");
}

#[test]
fn a_foreign_host_never_launches_on_any_path() {
    for target in ["/json/version", "/x", "/devtools/browser/x"] {
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: attacker.example:9222\r\nUpgrade: websocket\r\n\r\n"
        );
        assert_forbidden(request.as_bytes());
    }
    assert_forbidden(b"PUT /json/new?about:blank HTTP/1.1\r\nHost: attacker.example\r\n\r\n");
}

#[test]
fn a_request_from_a_web_page_never_launches() {
    for header in [
        "Origin: http://127.0.0.1",
        "origin: null",
        "Sec-Fetch-Site: cross-site",
        "Sec-Fetch-Dest: image",
        "sec-fetch-user: ?1",
    ] {
        for target in ["/json/version", "/devtools/browser/x"] {
            let request = format!(
                "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n{header}\r\n\r\n"
            );
            assert_forbidden(request.as_bytes());
        }
    }
}

#[test]
fn node_fetch_metadata_alone_is_admitted() {
    // Node's `fetch` sends `Sec-Fetch-Mode: cors` and nothing else of the
    // fetch metadata; puppeteer's discovery may run there.
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    let response = raw_roundtrip(
        proxy.port(),
        b"GET /json/version HTTP/1.1\r\nHost: 127.0.0.1\r\nsec-fetch-mode: cors\r\n\r\n",
    );
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"), "{response:?}");
}

#[test]
fn only_discovery_may_skip_the_websocket_upgrade() {
    for request in [
        "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
        "GET /devtools/browser/x HTTP/1.1\r\nHost: localhost\r\n\r\n",
        "GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: h2c\r\n\r\n",
        "GET /x HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: upgrade\r\n\r\n",
    ] {
        assert_forbidden(request.as_bytes());
    }
}

#[test]
fn discovery_rejects_duplicate_host_before_ensure() {
    assert_forbidden(b"GET /json/version HTTP/1.1\r\nHost: localhost\r\nhOsT: 127.0.0.1\r\n\r\n");
}

#[test]
fn missing_or_malformed_host_is_rejected_before_ensure() {
    for host in [
        "",
        "Host: \r\n",
        "Host : localhost\r\n",
        " Host: localhost\r\n",
        "Host localhost\r\n",
        "Host: localhost:\r\n",
        "Host: localhost:65536\r\n",
        "Host: localhost:abc\r\n",
        "Host: localhost,127.0.0.1\r\n",
        "Host: localhost.attacker.example\r\n",
        "Host: user@localhost\r\n",
        "Host: localhost/path\r\n",
        "Host: ::1\r\n",
        "Host: [::1]:abc\r\n",
        "Host: localhost\r\n folded\r\n",
    ] {
        for target in ["/json/version", "/devtools/browser/x"] {
            let request = format!("GET {target} HTTP/1.1\r\nUpgrade: websocket\r\n{host}\r\n");
            assert_forbidden(request.as_bytes());
        }
    }
}

#[test]
fn absolute_form_target_is_rejected_before_ensure() {
    assert_forbidden(b"GET http://localhost/json/version HTTP/1.1\r\nHost: localhost\r\n\r\n");
}

#[test]
fn websocket_upgrade_head_reaches_upstream_unchanged() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let request =
        b"GET /devtools/browser/x HTTP/1.1\r\nHost: localhost:9222\r\nUpgrade: websocket\r\nX-Extra: kept\r\n\r\n";
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut received = vec![0; request.len()];
        stream.read_exact(&mut received).unwrap();
        stream.write_all(b"unchanged").unwrap();
        received
    });
    let proxy = Proxy::bind(Arc::new(move || Ok(port)), nobody()).unwrap();
    assert_eq!(raw_roundtrip(proxy.port(), request), b"unchanged");
    assert_eq!(upstream.join().unwrap(), request);
}

#[test]
fn discovery_accepts_loopback_authorities_with_any_port() {
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    for host in [
        "localhost",
        "LoCaLhOsT:12345",
        "127.0.0.1",
        "127.0.0.1:0",
        "[::1]",
        "[::1]:65535",
    ] {
        let request =
            format!("GET /json/version HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
        let response = raw_roundtrip(proxy.port(), request.as_bytes());
        assert!(
            response.starts_with(b"HTTP/1.1 200 OK\r\n"),
            "{host}: {response:?}"
        );
        assert!(String::from_utf8_lossy(&response).contains(&format!(
            "ws://127.0.0.1:{}/devtools/browser/x",
            proxy.port()
        )));
    }
}

#[test]
fn ensure_error_is_a_503() {
    let proxy = Proxy::bind(Arc::new(|| Err("no chromium".into())), nobody()).unwrap();
    let raw = raw_roundtrip(
        proxy.port(),
        b"GET /json/version HTTP/1.1\r\nHost: localhost\r\n\r\n",
    );
    assert!(String::from_utf8_lossy(&raw).starts_with("HTTP/1.1 503"));
}

#[test]
fn oversized_head_is_closed_unanswered() {
    let (ensure, calls) = refusing_ensure();
    let proxy = Proxy::bind(ensure, nobody()).unwrap();
    let mut request = b"GET /json HTTP/1.1\r\nHost: localhost\r\nX: ".to_vec();
    request.resize(MAX_HEAD + 100, b'a');
    assert_eq!(raw_roundtrip(proxy.port(), &request), Vec::<u8>::new());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn connections_over_the_cap_are_refused_until_one_closes() {
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    // Idle clients: each holds a slot while the proxy waits for its head.
    let mut idle: Vec<TcpStream> = (0..MAX_CONNECTIONS)
        .map(|_| TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap())
        .collect();
    // Refused on the accept thread at once, before any request is read.
    let mut extra = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    extra
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut refused = Vec::new();
    let _ = extra.read_to_end(&mut refused);
    assert!(
        refused.starts_with(b"HTTP/1.1 503 Service Unavailable\r\n"),
        "{}",
        String::from_utf8_lossy(&refused)
    );
    drop(idle.pop());
    let served = eventually(Duration::from_secs(3), || {
        http_get(proxy.port(), "/json/version")
            .ok()
            .filter(|body| body.contains("webSocketDebuggerUrl"))
    });
    assert!(served.is_some(), "a freed slot serves again");
    drop(idle);
}

#[test]
fn finished_connections_are_untracked() {
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    for _ in 0..3 {
        http_get(proxy.port(), "/json/version").unwrap();
    }
    let _ = raw_roundtrip(proxy.port(), upgrade("/x").as_bytes());
    let _ = raw_roundtrip(proxy.port(), b"GET /x HTTP/1.1\r\n\r\n");
    let drained = eventually(Duration::from_secs(3), || {
        let idle =
            proxy.shared.live().is_empty() && proxy.shared.connections.load(Ordering::SeqCst) == 0;
        idle.then_some(())
    });
    assert!(
        drained.is_some(),
        "closed connections must leave the live map and free their slots"
    );
}

/// Upstream that reads until the client's EOF, then replies and closes.
fn reply_after_eof_upstream() -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut received = Vec::new();
                stream.read_to_end(&mut received).unwrap();
                write!(stream, "REPLY:{}", received.ends_with(b"PING")).unwrap();
            });
        }
    });
    port
}

#[test]
fn client_half_close_keeps_the_reply_path_open() {
    let up = reply_after_eof_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    s.write_all(format!("{}PING", upgrade("/devtools/x")).as_bytes())
        .unwrap();
    s.shutdown(Shutdown::Write).unwrap();
    let mut reply = Vec::new();
    s.read_to_end(&mut reply).unwrap();
    assert_eq!(reply, b"REPLY:true");
    let drained = eventually(Duration::from_secs(3), || {
        proxy.shared.live().is_empty().then_some(())
    });
    assert!(
        drained.is_some(),
        "finished half-closed splice must leave the live map"
    );
}

#[test]
fn trickled_head_is_cut_at_the_absolute_deadline() {
    let (ensure, _) = refusing_ensure();
    let proxy = Proxy::bind_with(ensure, nobody(), Duration::from_millis(300)).unwrap();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    let mut writer = s.try_clone().unwrap();
    // One byte every 100 ms: each read arrives well inside a per-read
    // timeout of 300 ms, yet the head never completes.
    let trickle = thread::spawn(move || {
        for _ in 0..40 {
            if writer.write_all(b"a").is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let start = Instant::now();
    let mut buf = [0u8; 16];
    let closed = match s.read(&mut buf) {
        Ok(n) => n == 0,
        Err(e) => !matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ),
    };
    assert!(
        closed,
        "proxy must close the connection, not time out the client"
    );
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "closed late: {:?}",
        start.elapsed()
    );
    trickle.join().unwrap();
}

#[test]
fn shutdown_cuts_live_splices() {
    let up = fake_upstream();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), nobody()).unwrap();
    let mut s = TcpStream::connect(("127.0.0.1", proxy.port())).unwrap();
    s.write_all(upgrade("/devtools/x").as_bytes()).unwrap();
    let spliced = eventually(Duration::from_secs(3), || {
        (proxy.shared.live().values().map(Vec::len).sum::<usize>() == 2).then_some(())
    });
    assert!(spliced.is_some(), "client and upstream are tracked");
    proxy.shutdown();
    s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut buf = [0u8; 64];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "socket closed");
    // The accept loop is gone: a new connection is refused or closed at once.
    match TcpStream::connect(("127.0.0.1", proxy.port())) {
        Err(_) => {}
        Ok(mut late) => {
            late.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = late.write_all(b"GET /json/version HTTP/1.1\r\nHost: localhost\r\n\r\n");
            assert!(
                matches!(late.read(&mut buf), Ok(0) | Err(_)),
                "no service after shutdown"
            );
        }
    }
}

const CREATE_TARGET: &str =
    r#"{"id":1,"method":"Target.createTarget","params":{"url":"about:blank"}}"#;
const CREATE_REPLY: &str = r#"{"id":1,"result":{"targetId":"T9"}}"#;

/// A stand-in for Chromium that accepts a WebSocket upgrade (`101` and
/// `extension_header`), waits for the client's first frame (`first_frame`
/// bytes), sends `before` (e.g. a screenshot reply), then `reply` once
/// `delay` has passed.
fn fake_chromium(
    extension_header: &'static str,
    first_frame: usize,
    before: Arc<Vec<u8>>,
    reply: &'static str,
    delay: Duration,
) -> u16 {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let before = Arc::clone(&before);
            thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                skip_head(&mut reader);
                write!(
                    stream,
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n{extension_header}\r\n"
                )
                .unwrap();
                let mut frame = vec![0; first_frame];
                reader.read_exact(&mut frame).unwrap();
                thread::sleep(delay);
                stream.write_all(&before).unwrap();
                stream.write_all(&text(reply)).unwrap();
                let _ = io::copy(&mut reader, &mut io::sink());
            });
        }
    });
    port
}

/// An agent that asks for a new page through a proxy in front of
/// [`fake_chromium`], which sends `before` ahead of the reply; returns what
/// the tap reported once the reply arrived and the bytes the agent got.
/// `split`: the first frame comes in a write of its own, not with the head.
fn agent_creates_a_page(
    extension_header: &'static str,
    split: bool,
    before: Vec<u8>,
) -> (Vec<String>, Vec<u8>) {
    let frame = masked_text(CREATE_TARGET);
    let up = fake_chromium(
        extension_header,
        frame.len(),
        Arc::new(before),
        CREATE_REPLY,
        Duration::from_millis(100),
    );
    let reported = Reported::default();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), reported.callback()).unwrap();
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let head = upgrade("/devtools/browser/x");
    if split {
        s.write_all(head.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(50));
        s.write_all(&frame).unwrap();
    } else {
        s.write_all(&[head.as_bytes(), &frame].concat()).unwrap();
    }
    let got = read_until_ends_with(&mut s, &text(CREATE_REPLY));
    // The tap is fed before the forward, but observes on its own thread.
    let _ = eventually(Duration::from_secs(1), || {
        (!reported.all().is_empty()).then_some(())
    });
    thread::sleep(Duration::from_millis(100)); // nothing more arrives
    (reported.all(), got)
}

#[test]
fn the_tap_sees_a_page_the_agent_creates_and_the_bytes_arrive_unchanged() {
    for split in [false, true] {
        let (reported, got) = agent_creates_a_page("", split, Vec::new());
        assert_eq!(reported, ["T9"], "split: {split}");
        assert!(
            got.starts_with(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n"),
            "{got:?}"
        );
    }
}

#[test]
fn a_large_reply_does_not_switch_follow_off() {
    // Two screenshot replies far bigger than the observer's queue.
    let screenshot = format!(
        r#"{{"id":0,"result":{{"data":"{}"}}}}"#,
        "A".repeat(6 << 20)
    );
    let mut before = text(&screenshot);
    before.extend(text(&screenshot));
    let (reported, got) = agent_creates_a_page("", false, before);
    assert_eq!(reported, ["T9"]);
    assert!(got.len() > 12 << 20, "the screenshots were spliced");
}

#[test]
fn a_negotiated_extension_blinds_the_tap_but_not_the_splice() {
    let (reported, got) = agent_creates_a_page(
        "Sec-WebSocket-Extensions: permessage-deflate\r\n",
        false,
        Vec::new(),
    );
    assert!(reported.is_empty(), "{reported:?}");
    assert!(got.ends_with(&text(CREATE_REPLY)));
}

const ATTACH: &str =
    r#"{"id":1,"method":"Target.attachToTarget","params":{"targetId":"T1","flatten":true}}"#;
const ATTACH_REPLY: &str = r#"{"id":1,"result":{"sessionId":"S1"}}"#;
const NAVIGATE: &str =
    r#"{"id":2,"method":"Page.navigate","params":{"url":"about:blank"},"sessionId":"S1"}"#;

/// An agent that attaches to `T1` through a Chromium that answers the
/// instant the attach frame is in, and the moment the reply is in, works in
/// the new session. The tap must have the session by then: the reply was
/// observed after the attach, the navigate after the reply. `split`: the
/// attach frame comes in a write of its own, not with the head.
/// `client_push_delay_ms` slows the client-side feed of this connection
/// (0: none). Returns what the tap reported.
fn attach_then_navigate_at_once(split: bool, client_push_delay_ms: u64) -> Vec<String> {
    let attach = masked_text(ATTACH);
    let up = fake_chromium(
        "",
        attach.len(),
        Arc::new(Vec::new()),
        ATTACH_REPLY,
        Duration::ZERO,
    );
    let reported = Reported::default();
    let proxy = Proxy::bind(Arc::new(move || Ok(up)), reported.callback()).unwrap();
    proxy.set_client_push_delay_ms(client_push_delay_ms);
    let mut s = TcpStream::connect((Ipv4Addr::LOCALHOST, proxy.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let head = upgrade("/devtools/browser/x");
    if split {
        s.write_all(head.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(50));
        s.write_all(&attach).unwrap();
    } else {
        s.write_all(&[head.as_bytes(), &attach].concat()).unwrap();
    }
    read_until_ends_with(&mut s, &text(ATTACH_REPLY));
    s.write_all(&masked_text(NAVIGATE)).unwrap();
    let _ = eventually(Duration::from_secs(3), || {
        (!reported.all().is_empty()).then_some(())
    });
    reported.all()
}

#[test]
fn a_reply_is_observed_after_its_command_even_when_the_client_push_is_slow() {
    // Were the command queued after it was forwarded, the 150 ms delay of
    // this connection's push would let the reply (and the dependent
    // navigate) overtake it.
    for split in [true, false] {
        assert_eq!(
            attach_then_navigate_at_once(split, 150),
            ["T1"],
            "split: {split}"
        );
    }
}

#[test]
fn an_instant_reply_never_overtakes_its_command() {
    for round in 0..40 {
        assert_eq!(
            attach_then_navigate_at_once(round % 2 == 0, 0),
            ["T1"],
            "round {round}"
        );
    }
}
