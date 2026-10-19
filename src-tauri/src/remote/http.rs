//! Minimal HTTP/1.1 plumbing for the remote server: bounded request-head
//! parsing, `Host`/`Origin` checks and response writing.

use std::io::{self, Read};
use std::time::{Duration, Instant};

pub mod admission;
pub mod response;

/// Largest request head (request line + headers + blank line) accepted.
pub const MAX_HEAD: usize = 16 * 1024;
/// Absolute time a client gets to deliver head and body.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(10);

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

#[cfg(test)]
#[path = "http_tests.rs"]
mod tests;
