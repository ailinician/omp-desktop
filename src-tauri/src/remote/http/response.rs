//! HTTP/1.1 response types, status phrases, CSP and writer.

use std::borrow::Cow;
use std::io::{self, Write};

pub const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'";

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
        503 => "Service Unavailable",
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
