use serde::Serialize;
use std::net::Ipv4Addr;

#[derive(Serialize, Clone, PartialEq, Eq, Debug)]
pub struct Iface {
    pub name: String,
    pub ip: Ipv4Addr,
}

#[cfg(unix)]
pub fn interfaces() -> Result<Vec<Iface>, String> {
    use std::ffi::CStr;

    struct Addresses(*mut libc::ifaddrs);
    impl Drop for Addresses {
        fn drop(&mut self) {
            // SAFETY: this guard owns the list returned by a successful getifaddrs.
            unsafe { libc::freeifaddrs(self.0) };
        }
    }

    let mut head = std::ptr::null_mut();
    // SAFETY: head is a valid writable pointer; the returned list lives until freeifaddrs.
    if unsafe { libc::getifaddrs(&raw mut head) } != 0 {
        return Err(format!(
            "list network interfaces: {}",
            std::io::Error::last_os_error()
        ));
    }
    let addresses = Addresses(head);
    let mut cursor = addresses.0;
    let mut list = Vec::new();
    while !cursor.is_null() {
        // SAFETY: cursor belongs to the live getifaddrs list.
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_addr.is_null()
            || entry.ifa_name.is_null()
            || entry.ifa_flags & libc::IFF_UP as u32 == 0
            || entry.ifa_flags & libc::IFF_LOOPBACK as u32 != 0
        {
            continue;
        }
        // SAFETY: getifaddrs supplies a sockaddr for each non-null address.
        if i32::from(unsafe { (*entry.ifa_addr).sa_family }) != libc::AF_INET {
            continue;
        }
        // SAFETY: AF_INET identifies a sockaddr_in, valid while addresses owns the list.
        let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
        let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
        if ip.is_loopback() || ip.is_unspecified() {
            continue;
        }
        // SAFETY: getifaddrs supplies a NUL-terminated name for the entry.
        let name = unsafe { CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        list.push(Iface { name, ip });
    }
    Ok(list)
}

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::interfaces;

#[cfg(not(any(unix, windows)))]
pub fn interfaces() -> Result<Vec<Iface>, String> {
    Err("remote access is not supported on this platform yet".into())
}

pub fn ipv4_of(name: &str, list: &[Iface]) -> Option<Ipv4Addr> {
    list.iter()
        .find(|interface| interface.name == name)
        .map(|interface| interface.ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_of_picks_the_named_interface_only() {
        let list = vec![
            Iface {
                name: "en0".into(),
                ip: "192.168.1.120".parse().unwrap(),
            },
            Iface {
                name: "bridge100".into(),
                ip: "192.168.139.3".parse().unwrap(),
            },
        ];
        assert_eq!(
            ipv4_of("en0", &list),
            Some("192.168.1.120".parse().unwrap())
        );
        assert_eq!(ipv4_of("utun4", &list), None);
    }

    #[cfg(unix)]
    #[test]
    fn interfaces_never_lists_loopback_or_unspecified() {
        let list = interfaces().unwrap();
        for i in &list {
            assert!(!i.ip.is_loopback() && !i.ip.is_unspecified(), "{i:?}");
        }

        // Observe host IPv4 availability independently: an always-empty result must fail.
        let mut head = std::ptr::null_mut();
        // SAFETY: head is writable; the returned list is freed below before asserting.
        assert_eq!(unsafe { libc::getifaddrs(&raw mut head) }, 0);
        let mut cursor = head;
        let mut has_ipv4 = false;
        while !cursor.is_null() {
            // SAFETY: cursor belongs to the live getifaddrs list.
            let entry = unsafe { &*cursor };
            cursor = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            // SAFETY: non-null getifaddrs addresses point to a valid sockaddr.
            if i32::from(unsafe { (*entry.ifa_addr).sa_family }) == libc::AF_INET {
                // SAFETY: AF_INET identifies sockaddr_in in the live list.
                let address = unsafe { &*entry.ifa_addr.cast::<libc::sockaddr_in>() };
                let ip = Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr));
                has_ipv4 |= !ip.is_loopback() && !ip.is_unspecified();
            }
        }
        // SAFETY: head is the list returned by the successful getifaddrs above.
        unsafe { libc::freeifaddrs(head) };
        if has_ipv4 {
            assert!(!list.is_empty(), "host has an active non-loopback IPv4");
        } else {
            eprintln!("skipping nonempty assertion: host has no active non-loopback IPv4");
        }
    }
}
