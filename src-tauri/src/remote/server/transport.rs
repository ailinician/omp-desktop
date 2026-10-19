//! Transport wrapper (TLS over TCP with absolute and per-call deadlines)
//! and per-IP / total in-flight connection slots.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustls::{ServerConnection, StreamOwned};

/// Accepted connection after the handshake-free part: the transport plus a
/// handle on the raw socket for timeouts.
pub struct Conn {
    pub stream: Transport,
    pub tcp: TcpStream,
    /// The connection slot of the physical socket, held until the WebSocket
    /// thread is done with it, so these limits also bound sockets the hub
    /// has already given up on.
    pub slot: Option<Slot>,
}

/// The TLS stream of one connection; the server never speaks plain HTTP.
pub struct Transport(Box<StreamOwned<ServerConnection, DeadlineSock>>);

impl Transport {
    pub(super) fn new(stream: StreamOwned<ServerConnection, DeadlineSock>) -> Self {
        Self(Box::new(stream))
    }

    /// Best-effort orderly shutdown (TLS `close_notify`, then TCP FIN).
    pub fn close(&mut self) {
        self.0.conn.send_close_notify();
        let _ = self.0.flush();
        let _ = self.0.sock.tcp.shutdown(Shutdown::Write);
    }

    /// Bytes received on the socket so far.
    pub const fn bytes_read(&self) -> u64 {
        self.0.sock.bytes_read
    }

    /// Bound all further socket I/O (TLS handshake included) by `deadline`;
    /// `None` leaves only the per-call caps.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.0.sock.deadline = deadline;
    }

    /// Longest a single read / write may block, within any deadline.
    pub fn set_caps(&mut self, read: Option<Duration>, write: Option<Duration>) {
        self.0.sock.read_cap = read;
        self.0.sock.write_cap = write;
    }

    pub(super) fn is_handshaking(&self) -> bool {
        self.0.conn.is_handshaking()
    }
}

impl Read for Transport {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Transport {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The socket under the transport. `rustls::Stream::read` (and a trickling
/// peer) may make many socket reads, so a per-read timeout is no deadline:
/// every call re-arms the socket with the time left, and fails once it is
/// spent. The caps bound a single call within that (the idle poll).
pub struct DeadlineSock {
    tcp: TcpStream,
    deadline: Option<Instant>,
    read_cap: Option<Duration>,
    write_cap: Option<Duration>,
    /// Bytes received so far, to tell an idle socket from a trickling one.
    bytes_read: u64,
}

impl DeadlineSock {
    pub(super) const fn new(tcp: TcpStream, deadline: Instant) -> Self {
        Self {
            tcp,
            deadline: Some(deadline),
            read_cap: None,
            write_cap: None,
            bytes_read: 0,
        }
    }

    /// Timeout for the next call: the time left to the deadline, capped.
    fn timeout(&self, cap: Option<Duration>) -> io::Result<Option<Duration>> {
        let left = match self.deadline {
            None => None,
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                Some(left)
            }
        };
        Ok(match (left, cap) {
            (Some(left), Some(cap)) => Some(left.min(cap)),
            (left, cap) => left.or(cap),
        })
    }
}

impl Read for DeadlineSock {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(timeout) = self.timeout(self.read_cap)? {
            self.tcp.set_read_timeout(Some(timeout))?;
        }
        let read = self.tcp.read(buf)?;
        self.bytes_read += read as u64;
        Ok(read)
    }
}

impl Write for DeadlineSock {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Some(timeout) = self.timeout(self.write_cap)? {
            self.tcp.set_write_timeout(Some(timeout))?;
        }
        self.tcp.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.tcp.flush()
    }
}

/// Connections accepted but not yet upgraded, in total and per peer address.
pub(super) struct InFlight {
    total: usize,
    per_ip: usize,
    peers: Mutex<HashMap<IpAddr, usize>>,
}

impl InFlight {
    pub(super) fn new(total: usize, per_ip: usize) -> Arc<Self> {
        Arc::new(Self {
            total,
            per_ip,
            peers: Mutex::new(HashMap::new()),
        })
    }

    /// A slot for `ip`, or `None` when a cap is reached.
    pub(super) fn admit(self: &Arc<Self>, ip: IpAddr) -> Option<Slot> {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mine = peers.get(&ip).copied().unwrap_or(0);
        if mine >= self.per_ip || peers.values().sum::<usize>() >= self.total {
            return None;
        }
        peers.insert(ip, mine + 1);
        drop(peers);
        Some(Slot {
            owner: Arc::clone(self),
            ip,
        })
    }
}

/// One in-flight connection, released on drop.
pub struct Slot {
    owner: Arc<InFlight>,
    ip: IpAddr,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut peers = self
            .owner
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(count) = peers.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                peers.remove(&self.ip);
            }
        }
    }
}
