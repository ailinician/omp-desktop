//! Minimal HTTP/1.1 plumbing for the remote server: bounded request-head
//! parsing, `Host`/`Origin` checks and response writing.

use std::borrow::Cow;
use std::io::{self, Read, Write};
use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

/// Largest request head (request line + headers + blank line) accepted.
pub const MAX_HEAD: usize = 16 * 1024;
/// Absolute time a client gets to deliver head and body.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'";

#[derive(Debug, PartialEq, Eq)]
pub enum HttpError {
    TooLarge,
    Timeout,
    Malformed,
    Closed,
    Io,
}

#[derive(Debug, Clone)]
pub struct Head {
    pub method: String,
    /// Request target without the query string.
    pub path: String,
    /// Lower-case names.
    headers: Vec<(String, String)>,
}

impl Head {
    /// `name` must be lower case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn is_websocket_upgrade(&self) -> bool {
        self.header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
    }

    /// `Content-Length`: `Ok(None)` when absent.
    pub fn content_length(&self) -> Result<Option<usize>, HttpError> {
        self.header("content-length").map_or(Ok(None), |value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpError::Malformed);
            }
            value.parse().map(Some).map_err(|_| HttpError::Malformed)
        })
    }
}

/// One read bounded by `deadline`. `arm` applies the remaining time to the socket.
fn fill<S: Read>(
    stream: &mut S,
    arm: &dyn Fn(Duration) -> io::Result<()>,
    deadline: Instant,
    buf: &mut [u8],
) -> Result<usize, HttpError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(HttpError::Timeout);
        }
        arm(remaining).map_err(|_| HttpError::Io)?;
        match stream.read(buf) {
            Ok(0) => return Err(HttpError::Closed),
            Ok(read) => return Ok(read),
            Err(error) => match error.kind() {
                io::ErrorKind::Interrupted => {}
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    return Err(HttpError::Timeout)
                }
                io::ErrorKind::UnexpectedEof => return Err(HttpError::Closed),
                _ => return Err(HttpError::Io),
            },
        }
    }
}

/// Read and parse one request head. Returns the head and any body bytes that
/// arrived in the same reads.
pub fn read_head<S: Read>(
    stream: &mut S,
    arm: &dyn Fn(Duration) -> io::Result<()>,
    deadline: Instant,
) -> Result<(Head, Vec<u8>), HttpError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let room = MAX_HEAD - buf.len();
        if room == 0 {
            return Err(HttpError::TooLarge);
        }
        let limit = room.min(chunk.len());
        let read = fill(stream, arm, deadline, &mut chunk[..limit])?;
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..read]);
        if let Some(offset) = buf[scan_from..]
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
        {
            let end = scan_from + offset + 4;
            let head = parse_head(&buf[..end - 4])?;
            return Ok((head, buf[end..].to_vec()));
        }
    }
}

/// Read exactly `len` body bytes, starting with what `read_head` already got.
pub fn read_body<S: Read>(
    stream: &mut S,
    arm: &dyn Fn(Duration) -> io::Result<()>,
    deadline: Instant,
    mut body: Vec<u8>,
    len: usize,
) -> Result<Vec<u8>, HttpError> {
    if body.len() > len {
        return Err(HttpError::Malformed);
    }
    let mut chunk = [0_u8; 1024];
    while body.len() < len {
        let limit = (len - body.len()).min(chunk.len());
        let read = fill(stream, arm, deadline, &mut chunk[..limit])?;
        body.extend_from_slice(&chunk[..read]);
    }
    Ok(body)
}

fn parse_head(raw: &[u8]) -> Result<Head, HttpError> {
    let text = std::str::from_utf8(raw).map_err(|_| HttpError::Malformed)?;
    let mut lines = text.split("\r\n");
    let request_line = lines.next().ok_or(HttpError::Malformed)?;
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HttpError::Malformed);
    };
    if method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
        || !target.starts_with('/')
    {
        return Err(HttpError::Malformed);
    }
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':').ok_or(HttpError::Malformed)?;
        if name.is_empty() || name.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(HttpError::Malformed);
        }
        let name = name.to_ascii_lowercase();
        // Ambiguous framing or authority: refuse instead of picking one.
        let unique = matches!(
            name.as_str(),
            "host" | "origin" | "content-length" | "transfer-encoding"
        );
        if name == "transfer-encoding" || (unique && headers.iter().any(|(seen, _)| *seen == name))
        {
            return Err(HttpError::Malformed);
        }
        headers.push((name, value.trim().to_owned()));
    }
    Ok(Head {
        method: method.to_owned(),
        path: path.to_owned(),
        headers,
    })
}

/// Which `Host` and `Origin` values belong to this server.
pub struct Allowed {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub host_name: Option<String>,
    /// Plain HTTP: origins are `http://…`, otherwise `https://…`.
    pub plain: bool,
}

impl Allowed {
    fn authority_matches(&self, candidate: &str) -> bool {
        let default_port = if self.plain { 80 } else { 443 };
        let (host, port) =
            candidate
                .rsplit_once(':')
                .map_or((candidate, Some(default_port)), |(host, port)| {
                    let digits = port.bytes().all(|byte| byte.is_ascii_digit());
                    (host, digits.then(|| port.parse::<u16>().ok()).flatten())
                });
        port == Some(self.port)
            && (host.eq_ignore_ascii_case(&self.ip.to_string())
                || self
                    .host_name
                    .as_deref()
                    .is_some_and(|name| host.eq_ignore_ascii_case(name)))
    }

    /// `Host` must name this server (defeats DNS rebinding).
    pub fn host_ok(&self, head: &Head) -> bool {
        head.header("host")
            .is_some_and(|host| self.authority_matches(host))
    }

    /// `Origin` must be the same origin as this server. A missing header is
    /// refused: every browser sends one on a WebSocket upgrade.
    pub fn origin_ok(&self, head: &Head) -> bool {
        let scheme = if self.plain { "http://" } else { "https://" };
        head.header("origin").is_some_and(|origin| {
            origin
                .get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
                && self.authority_matches(&origin[scheme.len()..])
        })
    }
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Cow<'static, [u8]>,
    pub extra: &'static [(&'static str, &'static str)],
}

impl Response {
    /// Empty-ish plain-text reply carrying just the status phrase.
    pub const fn status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: Cow::Borrowed(reason(status).as_bytes()),
            extra: &[],
        }
    }

    pub const fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: Cow::Owned(body),
            extra: &[],
        }
    }

    pub const fn with_extra(mut self, extra: &'static [(&'static str, &'static str)]) -> Self {
        self.extra = extra;
        self
    }
}

pub const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        410 => "Gone",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    }
}

/// Every response is uncacheable, sniff-proof and carries the CSP.
pub fn write_response<W: Write>(out: &mut W, response: &Response) -> io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nContent-Security-Policy: {CSP}\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n",
        response.status,
        reason(response.status),
        response.content_type,
        response.body.len(),
    );
    for (name, value) in response.extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    out.write_all(head.as_bytes())?;
    out.write_all(&response.body)?;
    out.flush()
}
