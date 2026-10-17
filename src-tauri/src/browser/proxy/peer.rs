//! Who is on the other end of a proxy connection: only a socket owned by the
//! OS user this app runs as is served. The proxy listens on loopback TCP,
//! which every account on the machine can reach; [`http::admit`] keeps web
//! pages out, this keeps other users out.
//!
//! The kernel's own socket tables say who owns the client's end of the
//! connection: `/proc/net/tcp` on Linux, `GetExtendedTcpTable` on Windows,
//! `lsof` on macOS. Anything that cannot be established is refused, and so
//! is every other OS.
//!
//! [`http::admit`]: super::http::admit

use std::net::TcpStream;

#[cfg(target_os = "macos")]
use lsof::same_user as owned_by_us;
#[cfg(target_os = "linux")]
use procfs::same_user as owned_by_us;
#[cfg(windows)]
use tcp_table::same_user as owned_by_us;

/// Whether the peer of `stream` (accepted by the proxy) is a socket of the
/// OS user this process runs as.
pub(super) fn same_user(stream: &TcpStream) -> bool {
    match (stream.peer_addr(), stream.local_addr()) {
        (Ok(peer), Ok(ours)) => owned_by_us(peer, ours),
        _ => false,
    }
}

/// No way to tell who owns a socket here: nobody is served.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
const fn owned_by_us(_peer: std::net::SocketAddr, _ours: std::net::SocketAddr) -> bool {
    false
}

/// Linux: the `uid` column of `/proc/net/tcp` (and `tcp6`, where a
/// dual-stack client's socket appears with a v4-mapped address).
#[cfg(any(test, target_os = "linux"))]
mod procfs {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    /// Whether the client's end (`peer` -> `ours`) and our own end
    /// (`ours` -> `peer`) of the connection are owned by the same uid.
    #[cfg(target_os = "linux")]
    pub(super) fn same_user(peer: SocketAddr, ours: SocketAddr) -> bool {
        let tables: Vec<String> = ["/proc/net/tcp", "/proc/net/tcp6"]
            .into_iter()
            .filter_map(|path| std::fs::read_to_string(path).ok())
            .collect();
        same_user_in(tables.iter().flat_map(|table| table.lines()), peer, ours)
    }

    /// [`same_user`] over the rows of the tables (header lines are skipped
    /// as rows that do not parse).
    pub(super) fn same_user_in<'a>(
        rows: impl IntoIterator<Item = &'a str>,
        peer: SocketAddr,
        ours: SocketAddr,
    ) -> bool {
        let (mut client, mut server) = (None, None);
        for (local, remote, uid) in rows.into_iter().filter_map(row) {
            if (local, remote) == (peer, ours) {
                client = Some(uid);
            } else if (local, remote) == (ours, peer) {
                server = Some(uid);
            }
        }
        client.is_some() && client == server
    }

    /// `sl local_address rem_address st tx:rx tr:when retrnsmt uid ...`
    fn row(line: &str) -> Option<(SocketAddr, SocketAddr, u32)> {
        let mut fields = line.split_whitespace().skip(1);
        let local = endpoint(fields.next()?)?;
        let remote = endpoint(fields.next()?)?;
        let uid = fields.nth(4)?.parse().ok()?;
        Some((local, remote, uid))
    }

    /// `ADDR:PORT`: the address as the kernel holds it (network order),
    /// printed one 32-bit word at a time as a native integer in hex; the
    /// port in host order.
    fn endpoint(field: &str) -> Option<SocketAddr> {
        let (addr, port) = field.split_once(':')?;
        let port = u16::from_str_radix(port, 16).ok()?;
        let ip = match addr.len() {
            8 => IpAddr::V4(Ipv4Addr::from(word(addr)?)),
            32 => {
                let mut octets = [0u8; 16];
                for (chunk, hex) in octets
                    .as_chunks_mut::<4>()
                    .0
                    .iter_mut()
                    .zip(addr.as_bytes().chunks(8))
                {
                    chunk.copy_from_slice(&word(std::str::from_utf8(hex).ok()?)?);
                }
                let v6 = Ipv6Addr::from(octets);
                v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4)
            }
            _ => return None,
        };
        Some(SocketAddr::new(ip, port))
    }

    fn word(hex: &str) -> Option<[u8; 4]> {
        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(hex, 16).ok().map(u32::to_ne_bytes)
    }
}

/// Windows: the client's end in the TCP table names the pid that created
/// the socket and when; that process's token user is compared with ours.
/// The pid may have exited and been reused since, by a process of any
/// user: a process created after the socket did not create it, so it is
/// refused.
#[cfg(any(test, windows))]
mod tcp_table {
    use std::net::{Ipv4Addr, SocketAddrV4};

    /// `u64` words of one `MIB_TCPROW_OWNER_MODULE` (160 bytes): six `u32`
    /// fields (state, local address, local port, remote address, remote
    /// port, owning pid) in the first three, `liCreateTimestamp` in the
    /// fourth, then `OwningModuleInfo` (`[u64; 16]`).
    pub(super) const ROW_WORDS: usize = 20;

    /// Who created a socket: its pid, and when (`liCreateTimestamp`, in
    /// FILETIME ticks: 100 ns since 1601).
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Owner {
        pub pid: u32,
        pub created: u64,
    }

    /// The owner of the row `local` -> `remote` of a
    /// `MIB_TCPTABLE_OWNER_MODULE` read as `u64`s (the entry count first,
    /// padded to 8 bytes).
    pub(super) fn owner(table: &[u64], local: SocketAddrV4, remote: SocketAddrV4) -> Option<Owner> {
        let (&header, rows) = table.split_first()?;
        let [count, _] = halves(header);
        rows.as_chunks::<ROW_WORDS>()
            .0
            .iter()
            .take(usize::try_from(count).ok()?)
            .find_map(|row| {
                let [_, local_addr] = halves(row[0]);
                let [local_port, remote_addr] = halves(row[1]);
                let [remote_port, pid] = halves(row[2]);
                (endpoint(local_addr, local_port) == local
                    && endpoint(remote_addr, remote_port) == remote)
                    .then_some(Owner {
                        pid,
                        created: row[3],
                    })
            })
    }

    /// The two `u32` fields a `u64` word of the table holds, in memory
    /// order.
    const fn halves(word: u64) -> [u32; 2] {
        let bytes = word.to_ne_bytes();
        [
            u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            u32::from_ne_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        ]
    }

    /// An address and port as the table holds them: both in network order,
    /// the port in the low 16 bits (its first two bytes in memory).
    fn endpoint(addr: u32, port: u32) -> SocketAddrV4 {
        let [high, low, ..] = port.to_ne_bytes();
        SocketAddrV4::new(
            Ipv4Addr::from(addr.to_ne_bytes()),
            u16::from_be_bytes([high, low]),
        )
    }

    #[cfg(windows)]
    pub(super) use ffi::same_user;

    #[cfg(windows)]
    mod ffi {
        use std::net::SocketAddr;
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use std::ptr::{addr_of_mut, null_mut};
        use std::sync::OnceLock;

        use windows_sys::Win32::Foundation::{
            ERROR_INSUFFICIENT_BUFFER, FILETIME, HANDLE, NO_ERROR,
        };
        use windows_sys::Win32::NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCPROW_OWNER_MODULE, TCP_TABLE_OWNER_MODULE_CONNECTIONS,
        };
        use windows_sys::Win32::Networking::WinSock::AF_INET;
        use windows_sys::Win32::Security::{
            EqualSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, GetProcessTimes, OpenProcess, OpenProcessToken,
            PROCESS_QUERY_LIMITED_INFORMATION,
        };

        use super::{owner, Owner, ROW_WORDS};

        const _: () = assert!(size_of::<MIB_TCPROW_OWNER_MODULE>() == ROW_WORDS * 8);

        /// Rows of slack when the table grew between sizing and reading.
        const SLACK_ROWS: usize = 16;

        pub fn same_user(peer: SocketAddr, ours: SocketAddr) -> bool {
            let (SocketAddr::V4(peer), SocketAddr::V4(ours)) = (peer, ours) else {
                return false;
            };
            let Some(Owner {
                pid,
                created: bound,
            }) = tcp_table().and_then(|table| owner(&table, peer, ours))
            else {
                return false;
            };
            // No creation time: who created the socket cannot be told from
            // whoever holds its pid now.
            if bound == 0 {
                return false;
            }
            let Some(us) = Us::get() else {
                return false;
            };
            if pid == std::process::id() {
                // Our pid since before the socket existed: ours.
                return us.created <= bound;
            }
            // SAFETY: plain-integer arguments.
            let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
            if process.is_null() {
                return false;
            }
            // SAFETY: a process handle just opened here and owned by nothing
            // else; `OwnedHandle` closes it exactly once.
            let process = unsafe { OwnedHandle::from_raw_handle(process) };
            // Created after the socket: the pid was reused, this process
            // did not create it.
            if created(process.as_raw_handle()).is_none_or(|created| created > bound) {
                return false;
            }
            let Some(theirs) = UserSid::of(process.as_raw_handle()) else {
                return false;
            };
            // SAFETY: both SIDs point into live `TOKEN_USER` buffers: `theirs`
            // owns its own until the end of this function, `us` is static.
            unsafe { EqualSid(theirs.sid(), us.user.sid()) != 0 }
        }

        /// The IPv4 connections with their owners, as `u64`s (the rows hold
        /// an `i64` and `u64`s, so the buffer is 8-byte aligned).
        fn tcp_table() -> Option<Vec<u64>> {
            let mut words = 0;
            for _ in 0..4 {
                let mut table = vec![0u64; words];
                let mut size = u32::try_from(words * 8).ok()?;
                // SAFETY: `table` is a live, 8-byte-aligned buffer of exactly
                // `size` bytes (empty on the first round, which only asks for
                // the size); `size` is a live `u32` the call updates. The
                // table class's rows are plain integers.
                let status = unsafe {
                    GetExtendedTcpTable(
                        table.as_mut_ptr().cast(),
                        addr_of_mut!(size),
                        0,
                        u32::from(AF_INET),
                        TCP_TABLE_OWNER_MODULE_CONNECTIONS,
                        0,
                    )
                };
                match status {
                    NO_ERROR => return Some(table),
                    ERROR_INSUFFICIENT_BUFFER => {
                        words = usize::try_from(size).ok()?.div_ceil(8) + SLACK_ROWS * ROW_WORDS;
                    }
                    _ => return None,
                }
            }
            None
        }

        /// When `process` was created, in FILETIME ticks.
        fn created(process: HANDLE) -> Option<u64> {
            let [mut creation, mut exit, mut kernel, mut user] = [FILETIME::default(); 4];
            // SAFETY: `process` is a valid process handle with query rights
            // (or the current-process pseudo-handle); the four FILETIMEs
            // are live locals the call writes.
            let read = unsafe {
                GetProcessTimes(
                    process,
                    addr_of_mut!(creation),
                    addr_of_mut!(exit),
                    addr_of_mut!(kernel),
                    addr_of_mut!(user),
                )
            };
            (read != 0).then_some(
                (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime),
            )
        }

        /// This process: its creation time and its token user, read once.
        struct Us {
            created: u64,
            user: UserSid,
        }

        static US: OnceLock<Us> = OnceLock::new();

        impl Us {
            /// `None` (and nothing kept, so the next check asks again) when
            /// either cannot be read: a fallible initializer, hence no
            /// `LazyLock`.
            fn get() -> Option<&'static Self> {
                if let Some(us) = US.get() {
                    return Some(us);
                }
                // SAFETY: no arguments; the pseudo-handle needs no closing.
                let own = unsafe { GetCurrentProcess() };
                let us = Self {
                    created: created(own)?,
                    user: UserSid::of(own)?,
                };
                Some(US.get_or_init(|| us))
            }
        }

        /// A process's `TOKEN_USER`, in a buffer aligned for it.
        struct UserSid(Vec<usize>);

        impl UserSid {
            fn of(process: HANDLE) -> Option<Self> {
                let mut token: HANDLE = null_mut();
                // SAFETY: `process` is a valid process handle (opened by the
                // caller, or the current-process pseudo-handle); `token` is a
                // live `HANDLE` the call writes, owned by the `OwnedHandle`
                // below.
                let opened = unsafe { OpenProcessToken(process, TOKEN_QUERY, addr_of_mut!(token)) };
                if opened == 0 {
                    return None;
                }
                // SAFETY: the call succeeded, so `token` is a token handle
                // just opened here and owned by nothing else; `OwnedHandle`
                // closes it exactly once.
                let token = unsafe { OwnedHandle::from_raw_handle(token) };
                let mut len = 0u32;
                // SAFETY: a size query: a null buffer of length 0; `len` is a
                // live `u32` the call writes. Its failure is expected.
                unsafe {
                    GetTokenInformation(
                        token.as_raw_handle(),
                        TokenUser,
                        null_mut(),
                        0,
                        addr_of_mut!(len),
                    )
                };
                let words = usize::try_from(len)
                    .ok()?
                    .div_ceil(std::mem::size_of::<usize>());
                if words == 0 {
                    return None;
                }
                let mut buf = vec![0usize; words];
                // SAFETY: `buf` is a live buffer of at least `len` bytes,
                // aligned for `TOKEN_USER` (pointer alignment); `len` is a
                // live `u32` the call writes.
                let read = unsafe {
                    GetTokenInformation(
                        token.as_raw_handle(),
                        TokenUser,
                        buf.as_mut_ptr().cast(),
                        len,
                        addr_of_mut!(len),
                    )
                };
                (read != 0).then_some(Self(buf))
            }

            const fn sid(&self) -> PSID {
                // SAFETY: the buffer starts with the `TOKEN_USER` that
                // `GetTokenInformation` wrote (checked in `of`), aligned for
                // it; its SID lives in the same buffer.
                unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
            }
        }
    }
}

/// macOS: `lsof` lists the client's end of the connection with its
/// process's uid. A non-root `lsof` sees no other user's sockets, so a
/// socket it does not list is refused too.
#[cfg(any(test, target_os = "macos"))]
mod lsof {
    /// Whether `lsof -F pun` output lists a socket named `name` held by a
    /// process running as `uid`. Fields are one per line, tagged by their
    /// first character: `p` starts a process, `u` is its uid, `f` a file
    /// descriptor, `n` the file's name.
    pub(super) fn lists(output: &str, name: &str, uid: u32) -> bool {
        let mut owner = None;
        for line in output.lines() {
            let (Some(tag), Some(value)) = (line.get(..1), line.get(1..)) else {
                continue;
            };
            match tag {
                "p" => owner = None,
                "u" => owner = value.parse::<u32>().ok(),
                "n" if value == name && owner == Some(uid) => return true,
                _ => {}
            }
        }
        false
    }

    #[cfg(target_os = "macos")]
    pub(super) use run::same_user;

    #[cfg(target_os = "macos")]
    mod run {
        use std::io::Read;
        use std::net::SocketAddr;
        use std::process::{Command, Stdio};
        use std::thread;
        use std::time::{Duration, Instant};

        use super::lists;

        /// How long `lsof` may take before the connection is refused.
        const TIMEOUT: Duration = Duration::from_secs(5);

        pub fn same_user(peer: SocketAddr, ours: SocketAddr) -> bool {
            let (SocketAddr::V4(peer), SocketAddr::V4(ours)) = (peer, ours) else {
                return false;
            };
            let Some(output) = lsof(&format!("-iTCP@{peer}")) else {
                return false;
            };
            // SAFETY: `geteuid` takes no arguments, cannot fail and changes
            // nothing.
            let uid = unsafe { libc::geteuid() };
            lists(&output, &format!("{peer}->{ours}"), uid)
        }

        /// The output of `/usr/sbin/lsof` (by absolute path: no `PATH`
        /// lookup) for the established TCP sockets matching `selection`;
        /// `None` when it cannot run or takes over [`TIMEOUT`].
        fn lsof(selection: &str) -> Option<String> {
            let mut child = Command::new("/usr/sbin/lsof")
                .args(["-nP", "-a", selection, "-sTCP:ESTABLISHED", "-Fpun"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let Some(mut stdout) = child.stdout.take() else {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            let reader = thread::Builder::new()
                .name("browser-proxy-lsof".into())
                .spawn(move || {
                    let mut output = String::new();
                    stdout.read_to_string(&mut output).map(|_| output)
                });
            let Ok(reader) = reader else {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            };
            let deadline = Instant::now() + TIMEOUT;
            let exited = loop {
                match child.try_wait() {
                    Ok(Some(_)) => break true,
                    Ok(None) if Instant::now() < deadline => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    _ => break false,
                }
            };
            if !exited {
                let _ = child.kill();
                let _ = child.wait();
            }
            // `lsof` exits 1 when it lists nothing: the output says so.
            let output = reader.join().ok()?.ok()?;
            exited.then_some(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

    use super::{lsof, procfs, tcp_table};

    const PEER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_123);
    const OURS: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 41_000);
    const OTHER: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::LOCALHOST, 50_124);

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn a_loopback_client_of_this_process_is_accepted() {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let _client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        assert!(super::same_user(&accepted));
    }

    /// A `/proc/net/tcp` word: the octets as the kernel prints them.
    fn hex_word(octets: [u8; 4]) -> String {
        format!("{:08X}", u32::from_ne_bytes(octets))
    }

    fn v4(addr: SocketAddrV4) -> String {
        format!("{}:{:04X}", hex_word(addr.ip().octets()), addr.port())
    }

    /// The v4-mapped form a dual-stack socket shows in `tcp6`.
    fn v6_mapped(addr: SocketAddrV4) -> String {
        let octets = addr.ip().to_ipv6_mapped().octets();
        let words: String = octets
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| hex_word(*w))
            .collect();
        format!("{words}:{:04X}", addr.port())
    }

    fn proc_row(local: &str, remote: &str, uid: u32) -> String {
        format!(
            "   0: {local} {remote} 01 00000000:00000000 00:00000000 00000000 {uid:>5}        0 4242 1 0000000000000000 20 4 30 10 -1"
        )
    }

    const PROC_HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";

    fn proc_same_user(rows: &[String]) -> bool {
        let lines = std::iter::once(PROC_HEADER).chain(rows.iter().map(String::as_str));
        procfs::same_user_in(lines, SocketAddr::V4(PEER), SocketAddr::V4(OURS))
    }

    #[test]
    fn procfs_accepts_a_client_of_our_uid() {
        assert!(proc_same_user(&[
            proc_row(&v4(OURS), &v4(PEER), 1000),
            proc_row(&v4(PEER), &v4(OURS), 1000),
        ]));
        // A dual-stack client shows up in `tcp6`, v4-mapped.
        assert!(proc_same_user(&[
            proc_row(&v4(OURS), &v4(PEER), 1000),
            proc_row(&v6_mapped(PEER), &v6_mapped(OURS), 1000),
        ]));
    }

    #[test]
    fn procfs_refuses_a_foreign_uid_or_a_missing_row() {
        assert!(!proc_same_user(&[
            proc_row(&v4(OURS), &v4(PEER), 1000),
            proc_row(&v4(PEER), &v4(OURS), 1001),
        ]));
        // Only some other connection of our uid.
        assert!(!proc_same_user(&[
            proc_row(&v4(OURS), &v4(PEER), 1000),
            proc_row(&v4(OTHER), &v4(OURS), 1000),
        ]));
        // Our own end alone says nothing about the client.
        assert!(!proc_same_user(&[proc_row(&v4(OURS), &v4(PEER), 1000)]));
        assert!(!proc_same_user(&[]));
    }

    /// `bytes` as the `u64` words they are in memory.
    fn words(bytes: &[u8]) -> Vec<u64> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|word| u64::from_ne_bytes(*word))
            .collect()
    }

    /// One `MIB_TCPROW_OWNER_MODULE` laid out as in memory, from host-order
    /// values; `OwningModuleInfo` is noise.
    fn table_row(local: SocketAddrV4, remote: SocketAddrV4, pid: u32, created: i64) -> Vec<u64> {
        let addr = |a: SocketAddrV4| a.ip().octets();
        let port = |a: SocketAddrV4| {
            let [high, low] = a.port().to_be_bytes();
            [high, low, 0, 0]
        };
        let mut bytes: Vec<u8> = [
            5u32.to_ne_bytes(),
            addr(local),
            port(local),
            addr(remote),
            port(remote),
            pid.to_ne_bytes(),
        ]
        .concat();
        bytes.extend(created.to_ne_bytes());
        bytes.resize(tcp_table::ROW_WORDS * 8, 0xAB);
        words(&bytes)
    }

    /// A `MIB_TCPTABLE_OWNER_MODULE`: the entry count, padding (noise), the
    /// rows.
    fn table(rows: &[Vec<u64>]) -> Vec<u64> {
        let mut header = u32::try_from(rows.len()).unwrap().to_ne_bytes().to_vec();
        header.resize(8, 0xCD);
        let mut table = words(&header);
        table.extend(rows.iter().flatten());
        table
    }

    #[test]
    fn the_tcp_table_names_the_owner_of_the_client_end_and_when_it_bound() {
        let owner = |pid, created| Some(tcp_table::Owner { pid, created });
        let rows = table(&[
            table_row(OURS, PEER, 1, 0x01DC_0000_1111_2222),
            table_row(OTHER, OURS, 2, 7),
            table_row(PEER, OURS, 3, 0x01DC_0000_3333_4444),
        ]);
        assert_eq!(
            tcp_table::owner(&rows, PEER, OURS),
            owner(3, 0x01DC_0000_3333_4444)
        );
        assert_eq!(
            tcp_table::owner(&rows, OURS, PEER),
            owner(1, 0x01DC_0000_1111_2222)
        );
        assert_eq!(tcp_table::owner(&rows, PEER, OTHER), None);
        // No timestamp is reported as such (the caller refuses it).
        let untimed = table(&[table_row(PEER, OURS, 3, 0)]);
        assert_eq!(tcp_table::owner(&untimed, PEER, OURS), owner(3, 0));
        // Rows past the entry count are not part of the table.
        let mut short = table(&[table_row(OURS, PEER, 1, 9)]);
        short.extend(table_row(PEER, OURS, 3, 9));
        assert_eq!(tcp_table::owner(&short, PEER, OURS), None);
        // Nor is a row cut short.
        let mut cut = table(&[table_row(PEER, OURS, 3, 9)]);
        cut.pop();
        assert_eq!(tcp_table::owner(&cut, PEER, OURS), None);
        assert_eq!(tcp_table::owner(&[], PEER, OURS), None);
    }

    fn lsof_output(records: &[(u32, u32, &str)]) -> String {
        use std::fmt::Write;
        records
            .iter()
            .fold(String::new(), |mut out, (pid, uid, name)| {
                let _ = write!(out, "p{pid}\nu{uid}\nf7\nn{name}\n");
                out
            })
    }

    #[test]
    fn lsof_accepts_only_the_client_end_held_by_our_uid() {
        let client = format!("{PEER}->{OURS}");
        let server = format!("{OURS}->{PEER}");
        let other = format!("{OTHER}->{OURS}");
        let (client, server, other) = (client.as_str(), server.as_str(), other.as_str());
        let ours = lsof_output(&[(10, 501, server), (20, 501, client)]);
        assert!(lsof::lists(&ours, client, 501));
        // Held by another user.
        let foreign = lsof_output(&[(10, 501, server), (20, 502, client)]);
        assert!(!lsof::lists(&foreign, client, 501));
        // Only our own end, or some other connection.
        assert!(!lsof::lists(
            &lsof_output(&[(10, 501, server)]),
            client,
            501
        ));
        assert!(!lsof::lists(&lsof_output(&[(20, 501, other)]), client, 501));
        // A uid from an earlier process record never carries over.
        assert!(!lsof::lists(
            &format!("p1\nu501\np2\nf3\nn{client}\n"),
            client,
            501
        ));
        assert!(!lsof::lists("", client, 501));
    }
}
