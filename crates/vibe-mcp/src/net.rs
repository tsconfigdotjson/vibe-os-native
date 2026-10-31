//! Where MCP listens: loopback, and the Tailscale address when that's on
//! (PRD: MCP interface, Transport; Auth, security and trust, Network
//! exposure). Never `0.0.0.0`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Whether `ip` is in Tailscale's ranges: `100.64.0.0/10` (its CGNAT
/// block) or `fd7a:115c:a1e0::/48`.
pub fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
    }
}

/// The addresses this machine's interfaces have now.
pub fn interface_addresses() -> Vec<IpAddr> {
    let mut out = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `head` with a list it allocated, which is
    // read here and freed once with freeifaddrs.
    unsafe {
        if libc::getifaddrs(&mut head) != 0 {
            return out;
        }
        let mut cur = head;
        while !cur.is_null() {
            let addr = (*cur).ifa_addr;
            if !addr.is_null() {
                match i32::from((*addr).sa_family) {
                    libc::AF_INET => {
                        let sin = &*(addr as *const libc::sockaddr_in);
                        out.push(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                            sin.sin_addr.s_addr,
                        ))));
                    }
                    libc::AF_INET6 => {
                        let sin6 = &*(addr as *const libc::sockaddr_in6);
                        out.push(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)));
                    }
                    _ => {}
                }
            }
            cur = (*cur).ifa_next;
        }
        libc::freeifaddrs(head);
    }
    out
}

/// This machine's Tailscale addresses, IPv4 first.
pub fn tailscale_addresses() -> Vec<IpAddr> {
    let mut found: Vec<IpAddr> = interface_addresses()
        .into_iter()
        .filter(|ip| is_tailscale(*ip))
        .collect();
    found.sort_by_key(|ip| (ip.is_ipv6(), *ip));
    found.dedup();
    found
}

/// The addresses to serve MCP on: loopback on `port`, and with `tailscale`
/// each Tailscale address there is now, on the same port.
pub fn listen_addresses(port: u16, tailscale: bool) -> Vec<SocketAddr> {
    listen_on(
        port,
        tailscale.then(tailscale_addresses).unwrap_or_default(),
    )
}

fn listen_on(port: u16, tailscale: Vec<IpAddr>) -> Vec<SocketAddr> {
    let mut out = vec![SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port)];
    out.extend(
        tailscale
            .into_iter()
            .filter(|ip| is_tailscale(*ip))
            .map(|ip| SocketAddr::new(ip, port)),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailscale_ranges() {
        for yes in [
            "100.64.0.1",
            "100.101.102.103",
            "100.127.255.254",
            "fd7a:115c:a1e0::1",
        ] {
            assert!(is_tailscale(yes.parse().unwrap()), "{yes}");
        }
        for no in [
            "100.63.255.255",
            "100.128.0.1",
            "127.0.0.1",
            "10.0.0.1",
            "0.0.0.0",
            "::1",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!is_tailscale(no.parse().unwrap()), "{no}");
        }
    }

    #[test]
    fn loopback_always_and_tailscale_when_on() {
        assert_eq!(
            listen_addresses(7878, false),
            ["127.0.0.1:7878".parse::<SocketAddr>().unwrap()]
        );
        let ts: IpAddr = "100.100.1.2".parse().unwrap();
        assert_eq!(
            listen_on(9000, vec![ts, "192.168.1.5".parse().unwrap()]),
            [
                "127.0.0.1:9000".parse::<SocketAddr>().unwrap(),
                "100.100.1.2:9000".parse().unwrap()
            ]
        );
        // Whatever the interfaces are, never a wildcard.
        for a in listen_addresses(7878, true) {
            assert!(!a.ip().is_unspecified(), "{a}");
        }
        assert!(interface_addresses().iter().any(|ip| ip.is_loopback()));
    }
}
