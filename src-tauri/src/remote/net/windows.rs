//! Windows adapter discovery; the shared manager still binds one selected IPv4.

use std::mem::MaybeUninit;
use std::ptr::null;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetAdaptersAddresses, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
    IF_TYPE_SOFTWARE_LOOPBACK, IP_ADAPTER_ADDRESSES_LH,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{
    IpDadStateDeprecated, IpDadStatePreferred, AF_INET, SOCKADDR_IN,
};

use super::{Iface, Ipv4Addr};

pub fn interfaces() -> Result<Vec<Iface>, String> {
    // Microsoft's recommended initial size avoids a sizing call on most hosts.
    let mut bytes = 15_000u32;
    for _ in 0..4 {
        // Allocate in adapter-sized units for the API's alignment, without
        // initialising storage the API overwrites. Pointers stay in this buffer.
        let units = (bytes as usize).div_ceil(size_of::<IP_ADAPTER_ADDRESSES_LH>());
        let mut buffer = Vec::<MaybeUninit<IP_ADAPTER_ADDRESSES_LH>>::with_capacity(units);
        let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
        // SAFETY: head is aligned and has at least `bytes` writable bytes;
        // bytes is writable, and the reserved argument must be null.
        let status = unsafe {
            GetAdaptersAddresses(
                u32::from(AF_INET),
                GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                null(),
                head,
                &raw mut bytes,
            )
        };
        match status {
            NO_ERROR => {
                // SAFETY: the successful call populated the linked adapter and
                // address lists and their strings within the still-live buffer.
                return Ok(unsafe { collect(head) });
            }
            ERROR_BUFFER_OVERFLOW => {} // adapters changed; bytes is the new size
            ERROR_NO_DATA => return Ok(Vec::new()),
            _ => {
                return Err(format!(
                    "list network interfaces: {}",
                    std::io::Error::from_raw_os_error(status.cast_signed())
                ))
            }
        }
    }
    Err("list network interfaces: adapter list kept changing".into())
}

/// # Safety
/// `cursor` must be null or a live `GetAdaptersAddresses` list, including its
/// unicast lists, socket addresses and NUL-terminated UTF-16 friendly names.
unsafe fn collect(mut cursor: *const IP_ADAPTER_ADDRESSES_LH) -> Vec<Iface> {
    let mut list = Vec::new();
    while !cursor.is_null() {
        // SAFETY: cursor belongs to the caller's live adapter list.
        let adapter = unsafe { &*cursor };
        cursor = adapter.Next;
        if adapter.OperStatus != IfOperStatusUp
            || adapter.IfType == IF_TYPE_SOFTWARE_LOOPBACK
            || adapter.FriendlyName.is_null()
        {
            continue;
        }
        let mut address = adapter.FirstUnicastAddress;
        while !address.is_null() {
            // SAFETY: the caller keeps this adapter's unicast list alive.
            let entry = unsafe { &*address };
            address = entry.Next;
            if entry.Address.lpSockaddr.is_null()
                || !usize::try_from(entry.Address.iSockaddrLength)
                    .is_ok_and(|length| length >= size_of::<SOCKADDR_IN>())
                || (entry.DadState != IpDadStatePreferred && entry.DadState != IpDadStateDeprecated)
            {
                continue;
            }
            // SAFETY: the caller provides a live socket address of at least
            // SOCKADDR_IN's size. SOCKADDR's type guarantees only 2-byte
            // alignment, so don't form a more strictly aligned reference.
            let socket = unsafe {
                entry
                    .Address
                    .lpSockaddr
                    .cast::<SOCKADDR_IN>()
                    .read_unaligned()
            };
            if socket.sin_family != AF_INET {
                continue;
            }
            // SAFETY: S_addr is the IPv4 representation of this AF_INET address;
            // its bytes are already in network order in memory.
            let ip = Ipv4Addr::from(unsafe { socket.sin_addr.S_un.S_addr }.to_ne_bytes());
            if ip.is_loopback() || ip.is_unspecified() {
                continue;
            }
            let mut length = 0;
            // SAFETY: FriendlyName is NUL-terminated in the caller's live list.
            while unsafe { *adapter.FriendlyName.add(length) } != 0 {
                length += 1;
            }
            // SAFETY: length excludes the terminating NUL of this live string.
            let name = String::from_utf16_lossy(unsafe {
                std::slice::from_raw_parts(adapter.FriendlyName, length)
            });
            list.push(Iface { name, ip });
        }
    }
    list
}

#[cfg(test)]
#[path = "windows_tests.rs"]
mod tests;
