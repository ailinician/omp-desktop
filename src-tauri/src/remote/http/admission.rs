//! `Host` and `Origin` admission checks for the remote server.

use std::net::Ipv4Addr;

use super::Head;

/// The port a `Host` or `Origin` without one stands for.
const HTTPS_PORT: u16 = 443;

/// Which `Host` and `Origin` values belong to this server.
pub struct Allowed {
    pub ip: Ipv4Addr,
    pub port: u16,
    pub host_name: Option<String>,
}

impl Allowed {
    fn authority_matches(&self, candidate: &str) -> bool {
        let (host, port) =
            candidate
                .rsplit_once(':')
                .map_or((candidate, Some(HTTPS_PORT)), |(host, port)| {
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
        let scheme = "https://";
        head.header("origin").is_some_and(|origin| {
            origin
                .get(..scheme.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
                && self.authority_matches(&origin[scheme.len()..])
        })
    }
}
