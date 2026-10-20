use super::*;
use std::ptr::null_mut;
use windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH;
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusDown;
use windows_sys::Win32::Networking::WinSock::{
    IpDadStateDuplicate, IpDadStateTentative, AF_INET6, IN_ADDR, IN_ADDR_0, SOCKADDR,
    SOCKET_ADDRESS,
};

#[test]
fn lists_named_ipv4_addresses_in_network_byte_order() {
    let mut name: Vec<u16> = "Ethernet 家 🖥".encode_utf16().chain(Some(0)).collect();
    let mut socket = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes([192, 168, 12, 34]),
            },
        },
        ..SOCKADDR_IN::default()
    };
    let mut other_socket = SOCKADDR_IN {
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes([10, 42, 0, 7]),
            },
        },
        ..socket
    };
    let mut second = IP_ADAPTER_UNICAST_ADDRESS_LH {
        Address: SOCKET_ADDRESS {
            lpSockaddr: (&raw mut other_socket).cast(),
            iSockaddrLength: i32::try_from(size_of::<SOCKADDR_IN>()).unwrap(),
        },
        DadState: IpDadStateDeprecated,
        ..IP_ADAPTER_UNICAST_ADDRESS_LH::default()
    };
    let mut first = IP_ADAPTER_UNICAST_ADDRESS_LH {
        Next: &raw mut second,
        DadState: IpDadStatePreferred,
        Address: SOCKET_ADDRESS {
            lpSockaddr: (&raw mut socket).cast(),
            iSockaddrLength: i32::try_from(size_of::<SOCKADDR_IN>()).unwrap(),
        },
        ..second
    };
    let adapter = IP_ADAPTER_ADDRESSES_LH {
        OperStatus: IfOperStatusUp,
        FriendlyName: name.as_mut_ptr(),
        FirstUnicastAddress: &raw mut first,
        ..IP_ADAPTER_ADDRESSES_LH::default()
    };
    // SAFETY: all list nodes, socket addresses and the terminated name are live locals.
    let list = unsafe { collect(&raw const adapter) };
    assert_eq!(
        list,
        vec![
            Iface {
                name: "Ethernet 家 🖥".into(),
                ip: Ipv4Addr::new(192, 168, 12, 34)
            },
            Iface {
                name: "Ethernet 家 🖥".into(),
                ip: Ipv4Addr::new(10, 42, 0, 7)
            },
        ]
    );
    assert_eq!(
        super::super::ipv4_of("Ethernet 家 🖥", &list),
        Some(Ipv4Addr::new(192, 168, 12, 34))
    );
    assert_eq!(super::super::ipv4_of("Wi-Fi", &list), None);
}

#[test]
fn ignores_down_loopback_and_nameless_adapters_but_keeps_vpn() {
    let mut name: Vec<u16> = "VPN".encode_utf16().chain(Some(0)).collect();
    let mut socket = SOCKADDR_IN {
        sin_family: AF_INET,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes([10, 8, 0, 2]),
            },
        },
        ..SOCKADDR_IN::default()
    };
    let mut address = IP_ADAPTER_UNICAST_ADDRESS_LH {
        Address: SOCKET_ADDRESS {
            lpSockaddr: (&raw mut socket).cast(),
            iSockaddrLength: i32::try_from(size_of::<SOCKADDR_IN>()).unwrap(),
        },
        DadState: IpDadStatePreferred,
        ..IP_ADAPTER_UNICAST_ADDRESS_LH::default()
    };
    let mut vpn = IP_ADAPTER_ADDRESSES_LH {
        OperStatus: IfOperStatusUp,
        IfType: 131, // IF_TYPE_TUNNEL: don't limit discovery to Ethernet/Wi-Fi.
        FriendlyName: name.as_mut_ptr(),
        FirstUnicastAddress: &raw mut address,
        ..IP_ADAPTER_ADDRESSES_LH::default()
    };
    let mut nameless = IP_ADAPTER_ADDRESSES_LH {
        Next: &raw mut vpn,
        FriendlyName: null_mut(),
        ..vpn
    };
    let mut loopback = IP_ADAPTER_ADDRESSES_LH {
        Next: &raw mut nameless,
        IfType: IF_TYPE_SOFTWARE_LOOPBACK,
        ..vpn
    };
    let down = IP_ADAPTER_ADDRESSES_LH {
        Next: &raw mut loopback,
        OperStatus: IfOperStatusDown,
        ..vpn
    };
    // SAFETY: the linked adapters, address and name all live through collect.
    assert_eq!(
        unsafe { collect(&raw const down) },
        vec![Iface {
            name: "VPN".into(),
            ip: Ipv4Addr::new(10, 8, 0, 2)
        }]
    );
}

#[test]
fn ignores_unusable_addresses_without_losing_later_valid_entries() {
    let mut name: Vec<u16> = "Ethernet".encode_utf16().chain(Some(0)).collect();
    let mut sockets: Vec<_> = [
        (AF_INET, [127, 0, 0, 1]),
        (AF_INET, [0, 0, 0, 0]),
        (AF_INET6, [192, 168, 0, 1]),
        (AF_INET, [192, 168, 0, 2]),
        (AF_INET, [192, 168, 0, 3]),
        (AF_INET, [192, 168, 0, 4]),
        (AF_INET, [192, 168, 0, 5]),
    ]
    .into_iter()
    .map(|(family, octets)| SOCKADDR_IN {
        sin_family: family,
        sin_addr: IN_ADDR {
            S_un: IN_ADDR_0 {
                S_addr: u32::from_ne_bytes(octets),
            },
        },
        ..SOCKADDR_IN::default()
    })
    .collect();
    let mut addresses: Vec<_> = sockets
        .iter_mut()
        .map(|socket| IP_ADAPTER_UNICAST_ADDRESS_LH {
            Address: SOCKET_ADDRESS {
                lpSockaddr: std::ptr::from_mut(socket).cast::<SOCKADDR>(),
                iSockaddrLength: i32::try_from(size_of::<SOCKADDR_IN>()).unwrap(),
            },
            DadState: IpDadStatePreferred,
            ..IP_ADAPTER_UNICAST_ADDRESS_LH::default()
        })
        .collect();
    addresses[3].DadState = IpDadStateTentative;
    addresses[4].DadState = IpDadStateDuplicate;
    addresses[5].Address.iSockaddrLength = 1;
    let mut next = null_mut();
    for address in addresses.iter_mut().rev() {
        address.Next = next;
        next = address;
    }
    let mut missing = IP_ADAPTER_UNICAST_ADDRESS_LH {
        Next: next,
        ..IP_ADAPTER_UNICAST_ADDRESS_LH::default()
    };
    let adapter = IP_ADAPTER_ADDRESSES_LH {
        OperStatus: IfOperStatusUp,
        FriendlyName: name.as_mut_ptr(),
        FirstUnicastAddress: &raw mut missing,
        ..IP_ADAPTER_ADDRESSES_LH::default()
    };
    // SAFETY: vectors aren't resized after linking; every node/address/name is live.
    assert_eq!(
        unsafe { collect(&raw const adapter) },
        vec![Iface {
            name: "Ethernet".into(),
            ip: Ipv4Addr::new(192, 168, 0, 5)
        }]
    );
}
