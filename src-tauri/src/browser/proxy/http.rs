//! The HTTP side of the proxy: reading and vetting a client's request head
//! before anything is launched, and relaying `/json*` discovery with the
//! CDP WebSocket URLs rewritten to point back at the proxy.

use std::fmt::Write as _;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// Longest request head a client may send.
pub(super) const MAX_HEAD: usize = 16 * 1024;
const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// A request head that passed [`admit`].
pub(super) struct Admitted<'a> {
    /// The head without its terminating blank line.
    pub head: &'a str,
    /// A `GET /json*` discovery request: relayed with rewritten URLs.
    /// Everything else is a WebSocket upgrade or other `/json*` request,
    /// spliced untouched.
    pub discovery: bool,
}

/// Vets `head` (without its terminator) before the lazy launch, so nothing
/// a web page can send starts a Chromium: an origin-form target, exactly one
/// well-formed loopback `Host` (DNS rebinding), no header only a browser
/// sends, and outside `/json*` a WebSocket upgrade. `Err` is the reason.
pub(super) fn admit(head: &[u8]) -> Result<Admitted<'_>, &'static str> {
    let head = std::str::from_utf8(head).map_err(|_| "request head is not text")?;
    let mut lines = head.split("\r\n");
    let mut request = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (request.next().unwrap_or_default(), request.next());
    let Some(target) = target.filter(|t| t.starts_with('/')) else {
        return Err("origin-form target required");
    };
    let (mut host, mut browser, mut upgrade) = (None, false, false);
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err("malformed header");
        };
        if !valid_header(name, value) {
            return Err("malformed header");
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("host") {
            if host.replace(value).is_some() {
                return Err("loopback Host required");
            }
        } else if from_browser(name) {
            browser = true;
        } else if name.eq_ignore_ascii_case("upgrade") {
            upgrade |= value
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("websocket"));
        }
    }
    if !host.is_some_and(loopback_authority) {
        return Err("loopback Host required");
    }
    if browser {
        return Err("requests from a web browser are refused");
    }
    let json = target.starts_with("/json");
    if !json && !upgrade {
        return Err("WebSocket upgrade required");
    }
    Ok(Admitted {
        head,
        discovery: json && method == "GET",
    })
}

/// A field name made of token characters and a value without control bytes
/// (so no folded or smuggled lines).
fn valid_header(name: &str, value: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !value.bytes().any(|b| b.is_ascii_control() && b != b'\t')
}

/// A header only a web browser sends: `Origin` (cross-origin fetches and
/// every WebSocket from a page) or the fetch metadata a browser attaches to
/// every request. `Sec-Fetch-Mode` alone is allowed: Node's `fetch`
/// (undici) sends it, and puppeteer may run there; a browser always adds
/// `Sec-Fetch-Site` too.
fn from_browser(name: &str) -> bool {
    let fetch_metadata = name
        .get(..10)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("sec-fetch-"));
    name.eq_ignore_ascii_case("origin")
        || (fetch_metadata && !name.eq_ignore_ascii_case("sec-fetch-mode"))
}

/// Whitelist the literal hosts Chromium accepts, with an optional numeric port.
fn loopback_authority(authority: &str) -> bool {
    let (host, suffix) = authority.strip_prefix("[::1]").map_or_else(
        || {
            authority
                .find(':')
                .map_or((authority, ""), |colon| authority.split_at(colon))
        },
        |suffix| ("[::1]", suffix),
    );
    (host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]")
        && (suffix.is_empty()
            || suffix.strip_prefix(':').is_some_and(|port| {
                !port.is_empty()
                    && port.bytes().all(|b| b.is_ascii_digit())
                    && port.parse::<u16>().is_ok()
            }))
}

/// Reads up to the blank line ending the request head. Returns the bytes read
/// so far (the head plus anything the client sent after it) and the head's
/// length including the terminator; `None` on EOF or a head over the cap.
/// The whole head must arrive by `deadline`, however slowly it trickles in.
pub(super) fn read_head(
    stream: &mut TcpStream,
    deadline: Instant,
) -> io::Result<Option<(Vec<u8>, usize)>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_read_timeout(Some(remaining))?;
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        // The terminator may straddle the previous chunk.
        let from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf[from..].windows(4).position(|w| w == b"\r\n\r\n") {
            let head_len = from + pos + 4;
            return Ok((head_len <= MAX_HEAD).then_some((buf, head_len)));
        }
        if buf.len() > MAX_HEAD {
            return Ok(None);
        }
    }
}

pub(super) fn respond(stream: &mut TcpStream, status: &str, body: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Replaces `127.0.0.1:{from}` and `localhost:{from}` with `127.0.0.1:{to}`.
pub(super) fn rewrite_json(body: &str, from: u16, to: u16) -> String {
    let target = format!("127.0.0.1:{to}");
    body.replace(&format!("127.0.0.1:{from}"), &target)
        .replace(&format!("localhost:{from}"), &target)
}

/// `head` is the admitted discovery request without its terminator; `ports`
/// are Chromium's and the proxy's own. Only now may Host point at Chromium.
pub(super) fn relay_json(
    client: &mut TcpStream,
    mut upstream: TcpStream,
    head: &str,
    ports: (u16, u16),
) -> io::Result<()> {
    let (chromium, own) = ports;
    upstream.set_read_timeout(Some(UPSTREAM_TIMEOUT))?;
    let mut request = String::with_capacity(head.len() + 24);
    for line in head.split("\r\n").filter(|l| !has_header(l, "host")) {
        request.push_str(line);
        request.push_str("\r\n");
    }
    let _ = write!(request, "Host: 127.0.0.1:{chromium}\r\n\r\n");
    upstream.write_all(request.as_bytes())?;

    // Chromium ignores `Connection: close`: read by `Content-Length`, not EOF.
    let mut reader = BufReader::new(upstream);
    let mut lines = Vec::new();
    let mut length = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if has_header(line, "content-length") {
            length = line
                .split_once(':')
                .and_then(|(_, v)| v.trim().parse().ok());
        } else if !has_header(line, "connection") {
            lines.push(line.to_owned());
        }
    }
    let length: usize = length.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "upstream sent no Content-Length",
        )
    })?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body =
        String::from_utf8(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let body = rewrite_json(&body, chromium, own);

    let mut response = lines.join("\r\n");
    let _ = write!(
        response,
        "\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    client.write_all(response.as_bytes())?;
    Ok(())
}

fn has_header(line: &str, name: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(n, _)| n.trim().eq_ignore_ascii_case(name))
}
