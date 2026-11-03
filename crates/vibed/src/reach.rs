//! Why `ssh` couldn't connect to a server (PRD: Adding a Linux host).
//!
//! `ssh` tries each of a server's addresses in turn and reports only the
//! last one's error. A server whose IPv6 address has no route from this
//! network therefore reads "No route to host", whatever really went wrong
//! over IPv4: a refused connection, a firewall, a server that's down. So
//! when `ssh` can't connect, `vibed` connects to each address itself and
//! says what happened.
//!
//! On a Mac, an address on the local network also needs the app's Local
//! Network access: macOS counts `vibed` and the `ssh` it runs as VibeOS's,
//! and refuses them without it. That gets a sentence of its own, saying how
//! to turn the access on, rather than "can't reach".

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

/// Where Local Network access is turned on.
pub const LOCAL_NETWORK_SETTINGS: &str =
    "System Settings > Privacy & Security > Local Network > VibeOS";

/// How long each address has to answer, and the name to resolve.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The most addresses tried.
const MOST_ADDRESSES: usize = 4;

/// How one of a server's addresses answered a connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Connected,
    Refused,
    TimedOut,
    /// No route to it from this network (`EHOSTUNREACH`, `ENETUNREACH`):
    /// also how macOS refuses an app without Local Network access an
    /// address on the local network.
    NoRoute,
    /// Not allowed (`EPERM`, `EACCES`).
    NotAllowed,
    /// On the local network, but nothing answered there (`EHOSTDOWN`).
    Down,
    Other(String),
}

impl Answer {
    /// The answer a failed connection's error stands for.
    pub fn of(e: &io::Error) -> Answer {
        match e.raw_os_error() {
            Some(libc::ECONNREFUSED) => Answer::Refused,
            Some(libc::ETIMEDOUT) => Answer::TimedOut,
            Some(libc::EHOSTUNREACH | libc::ENETUNREACH) => Answer::NoRoute,
            Some(libc::EPERM | libc::EACCES) => Answer::NotAllowed,
            Some(libc::EHOSTDOWN) => Answer::Down,
            _ if e.kind() == io::ErrorKind::TimedOut => Answer::TimedOut,
            _ => Answer::Other(e.to_string()),
        }
    }
}

/// An address tried, whether it's on the local network, and its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tried {
    pub ip: IpAddr,
    pub local: bool,
    pub answer: Answer,
}

/// What `ssh` connects to for a destination: from `ssh -G`'s output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
}

/// The host and port `ssh -G` says a destination connects to, or `None`
/// when `ssh` goes through a proxy (`ProxyJump`, `ProxyCommand`): then it
/// doesn't connect to the host itself, and neither should a check.
pub fn target(config: &str) -> Option<Target> {
    let mut host = None;
    let mut port = 22;
    for line in config.lines() {
        let Some((key, value)) = line.trim().split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key.to_ascii_lowercase().as_str() {
            "hostname" => host = Some(value.to_owned()),
            "port" => port = value.parse().ok()?,
            "proxyjump" | "proxycommand" if !value.eq_ignore_ascii_case("none") => return None,
            _ => {}
        }
    }
    Some(Target {
        host: host.filter(|h| !h.is_empty())?,
        port,
    })
}

/// Connects to each of `target`'s addresses, at once, and says how each
/// answered. Empty when the name doesn't resolve.
pub async fn try_addresses(target: &Target) -> Vec<Tried> {
    let resolved = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::net::lookup_host((target.host.as_str(), target.port)),
    )
    .await;
    let mut addresses: Vec<SocketAddr> = match resolved {
        Ok(Ok(found)) => found.collect(),
        _ => return Vec::new(),
    };
    addresses.dedup();
    addresses.truncate(MOST_ADDRESSES);
    // Together, each bounded by its timeout; kept in the resolver's order.
    let mut set = tokio::task::JoinSet::new();
    for (i, address) in addresses.into_iter().enumerate() {
        set.spawn(async move {
            let answer =
                match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(address))
                    .await
                {
                    Ok(Ok(_)) => Answer::Connected,
                    Ok(Err(e)) => Answer::of(&e),
                    Err(_) => Answer::TimedOut,
                };
            let ip = address.ip();
            let local = tokio::task::spawn_blocking(move || is_local(ip))
                .await
                .unwrap_or(false);
            (i, Tried { ip, local, answer })
        });
    }
    let mut tried: Vec<(usize, Tried)> = set.join_all().await;
    tried.sort_by_key(|(i, _)| *i);
    tried.into_iter().map(|(_, t)| t).collect()
}

/// What to tell the owner when `ssh` couldn't connect to `address`, from
/// what connecting to each address found: `None` when that adds nothing to
/// what `ssh` said. `mac` is whether this is a Mac, where Local Network
/// access applies.
pub fn explain(address: &str, target: &Target, tried: &[Tried], mac: bool) -> Option<String> {
    let port = target.port;
    // The host, and the address of it that answered so: just the address
    // when the host is one.
    let at = |ip: IpAddr| {
        if target.host.parse::<IpAddr>() == Ok(ip) {
            ip.to_string()
        } else {
            format!("{} ({ip})", target.host)
        }
    };
    if let Some(t) = tried.iter().find(|t| t.answer == Answer::Connected) {
        let at = at(t.ip);
        return Some(format!(
            "can't reach {address} over SSH: ssh couldn't connect, though {at} answers \
             on port {port} now. A server that limits how often a client may connect (ufw limit, \
             fail2ban, sshd's MaxStartups) turns connections away for a while: wait a minute, \
             then try again."
        ));
    }
    if mac
        && let Some(t) = tried
            .iter()
            .find(|t| t.local && matches!(t.answer, Answer::NoRoute | Answer::NotAllowed))
    {
        return Some(local_network_off(address, t.ip));
    }
    // The address that says what's wrong: one that has a route from here.
    let telling = tried.iter().find(|t| t.answer != Answer::NoRoute)?;
    let at = at(telling.ip);
    let (what, hint) = match &telling.answer {
        Answer::Refused => (
            "refused the connection",
            " Check that sshd runs there and that its firewall lets this Mac in. A server that \
             lets a few connections in and then refuses more is limiting how often a client may \
             connect (ufw limit or fail2ban, say): wait a minute, then try again."
                .to_owned(),
        ),
        Answer::TimedOut => (
            "didn't answer",
            " The server may be down, or a firewall is dropping connections to it.".to_owned(),
        ),
        Answer::Down => (
            "didn't answer",
            " Nothing answers at that address on the local network: is the server on?".to_owned(),
        ),
        Answer::NotAllowed => (
            "couldn't be reached",
            " This Mac didn't allow the connection: a firewall or content filter here may be \
             blocking it."
                .to_owned(),
        ),
        Answer::Other(e) => ("couldn't be reached", format!(" Connecting said: {e}.")),
        Answer::Connected | Answer::NoRoute => return None,
    };
    let unrouted: Vec<String> = tried
        .iter()
        .filter(|t| t.answer == Answer::NoRoute)
        .map(|t| t.ip.to_string())
        .collect();
    let also = match unrouted.as_slice() {
        [] => String::new(),
        [one] => format!(" ({one} has no route from this network)"),
        many => format!(" ({} have no route from this network)", many.join(", ")),
    };
    Some(format!(
        "can't reach {address} over SSH: {at} {what} on port {port}{also}.{hint}"
    ))
}

/// Local Network access is off: what to say, and how to turn it on.
pub fn local_network_off(address: &str, ip: IpAddr) -> String {
    format!(
        "Local Network access is off for VibeOS, so it can't connect to {address} ({ip}) on your \
         local network. Turn it on in {LOCAL_NETWORK_SETTINGS}, then try again."
    )
}

/// Whether `ip` is on a local network as macOS's Local Network privacy
/// counts it: on a network of one of this machine's broadcast-capable
/// interfaces (Wi-Fi, Ethernet; not loopback, not a VPN's), link-local, or
/// multicast or broadcast. Traffic to it goes directly, not through a router.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) if a.is_multicast() || a.is_link_local() || a == Ipv4Addr::BROADCAST => {
            return true;
        }
        IpAddr::V6(a) if a.is_multicast() || is_link_local_v6(a) => return true,
        _ => {}
    }
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    local_networks()
        .iter()
        .any(|(net, mask)| same_network(ip, *net, *mask))
}

fn is_link_local_v6(a: Ipv6Addr) -> bool {
    a.segments()[0] & 0xffc0 == 0xfe80
}

/// Whether `ip` is on the network `net` with `mask`.
pub fn same_network(ip: IpAddr, net: IpAddr, mask: IpAddr) -> bool {
    match (ip, net, mask) {
        (IpAddr::V4(ip), IpAddr::V4(net), IpAddr::V4(mask)) => {
            let m = u32::from(mask);
            m != 0 && u32::from(ip) & m == u32::from(net) & m
        }
        (IpAddr::V6(ip), IpAddr::V6(net), IpAddr::V6(mask)) => {
            let m = u128::from(mask);
            m != 0 && u128::from(ip) & m == u128::from(net) & m
        }
        _ => false,
    }
}

/// The networks of this machine's interfaces that are up and can
/// broadcast, but aren't loopback or point-to-point (a VPN's): each an
/// address with its mask.
pub fn local_networks() -> Vec<(IpAddr, IpAddr)> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills `list` with a list it allocated, which is
    // read only until freeifaddrs below frees it. Each sockaddr is read as
    // the type its family says it is.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut at = list;
        while !at.is_null() {
            let i = &*at;
            at = i.ifa_next;
            let flags = i.ifa_flags as libc::c_int;
            let wanted = flags & libc::IFF_UP != 0
                && flags & libc::IFF_BROADCAST != 0
                && flags & (libc::IFF_LOOPBACK | libc::IFF_POINTOPOINT) == 0;
            if !wanted || i.ifa_addr.is_null() || i.ifa_netmask.is_null() {
                continue;
            }
            if let (Some(addr), Some(mask)) = (ip_of(i.ifa_addr), ip_of(i.ifa_netmask)) {
                found.push((addr, mask));
            }
        }
        libc::freeifaddrs(list);
    }
    found
}

/// The address in `sa`, an IPv4 or IPv6 one.
///
/// # Safety
/// `sa` points to a valid sockaddr of the size its family says.
unsafe fn ip_of(sa: *const libc::sockaddr) -> Option<IpAddr> {
    // SAFETY: the caller's; the family says which type to read it as.
    unsafe {
        match i32::from((*sa).sa_family) {
            libc::AF_INET => {
                let sin = &*(sa.cast::<libc::sockaddr_in>());
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            }
            libc::AF_INET6 => {
                let sin6 = &*(sa.cast::<libc::sockaddr_in6>());
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn vps() -> Target {
        Target {
            host: "vps.example.com".into(),
            port: 22,
        }
    }

    fn tried(s: &str, local: bool, answer: Answer) -> Tried {
        Tried {
            ip: ip(s),
            local,
            answer,
        }
    }

    #[test]
    fn errors_are_the_answers_they_stand_for() {
        let of = |n| Answer::of(&io::Error::from_raw_os_error(n));
        assert_eq!(of(libc::ECONNREFUSED), Answer::Refused);
        assert_eq!(of(libc::ETIMEDOUT), Answer::TimedOut);
        assert_eq!(of(libc::EHOSTUNREACH), Answer::NoRoute);
        assert_eq!(of(libc::ENETUNREACH), Answer::NoRoute);
        assert_eq!(of(libc::EPERM), Answer::NotAllowed);
        assert_eq!(of(libc::EHOSTDOWN), Answer::Down);
    }

    #[test]
    fn the_target_is_what_ssh_g_says() {
        let config = "user me\nhostname vps.example.com\nport 2222\naddressfamily any\n\
                      proxycommand none\n";
        assert_eq!(
            target(config),
            Some(Target {
                host: "vps.example.com".into(),
                port: 2222
            })
        );
        // Through a jump host, ssh doesn't connect to it directly.
        assert_eq!(target("hostname vps\nport 22\nproxyjump bastion\n"), None);
        assert_eq!(target("hostname vps\nproxycommand nc %h %p\n"), None);
        assert_eq!(target("port 22\n"), None);
    }

    /// The case from issue #39: IPv4 refused, then IPv6 with no route,
    /// which is all ssh said.
    #[test]
    fn an_unrouted_ipv6_address_doesnt_hide_what_ipv4_said() {
        let m = explain(
            "me@vps",
            &vps(),
            &[
                tried("54.39.97.232", false, Answer::Refused),
                tried("2607:5300:205:200::8b3e", false, Answer::NoRoute),
            ],
            true,
        )
        .unwrap();
        assert!(
            m.starts_with(
                "can't reach me@vps over SSH: vps.example.com (54.39.97.232) refused the \
                 connection on port 22 (2607:5300:205:200::8b3e has no route from this network)."
            ),
            "{m}"
        );
        assert!(m.contains("wait a minute"), "{m}");
        assert!(!m.contains("Local Network"), "{m}");

        let m = explain(
            "me@vps",
            &vps(),
            &[
                tried("2607:5300:205:200::8b3e", false, Answer::NoRoute),
                tried("54.39.97.232", false, Answer::TimedOut),
            ],
            true,
        )
        .unwrap();
        assert!(
            m.starts_with(
                "can't reach me@vps over SSH: vps.example.com (54.39.97.232) didn't answer on port 22"
            ),
            "{m}"
        );
    }

    #[test]
    fn a_local_address_turned_away_is_local_network_access() {
        let m = explain(
            "me@nas",
            &vps(),
            &[tried("192.168.2.20", true, Answer::NoRoute)],
            true,
        )
        .unwrap();
        assert_eq!(
            m,
            "Local Network access is off for VibeOS, so it can't connect to me@nas \
             (192.168.2.20) on your local network. Turn it on in System Settings > Privacy & \
             Security > Local Network > VibeOS, then try again."
        );
        // Off a Mac there's no such thing; nor for an address out there.
        assert_eq!(
            explain(
                "me@nas",
                &vps(),
                &[tried("192.168.2.20", true, Answer::NoRoute)],
                false
            ),
            None
        );
        assert_eq!(
            explain(
                "me@vps",
                &vps(),
                &[tried("54.39.97.232", false, Answer::NoRoute)],
                true
            ),
            None
        );
    }

    #[test]
    fn a_host_given_as_an_address_is_named_once() {
        let m = explain(
            "me@10.1.2.3",
            &Target {
                host: "10.1.2.3".into(),
                port: 2222,
            },
            &[tried("10.1.2.3", false, Answer::Refused)],
            true,
        )
        .unwrap();
        assert!(
            m.starts_with(
                "can't reach me@10.1.2.3 over SSH: 10.1.2.3 refused the connection on port 2222."
            ),
            "{m}"
        );
    }

    #[test]
    fn a_server_that_answers_now_was_turning_connections_away() {
        let m = explain(
            "me@vps",
            &vps(),
            &[tried("54.39.97.232", false, Answer::Connected)],
            true,
        )
        .unwrap();
        assert!(m.contains("answers on port 22 now"), "{m}");
        assert!(m.contains("wait a minute"), "{m}");
    }

    #[test]
    fn networks_match_by_their_mask() {
        assert!(same_network(
            ip("192.168.2.20"),
            ip("192.168.2.88"),
            ip("255.255.255.0")
        ));
        assert!(!same_network(
            ip("54.39.97.232"),
            ip("192.168.2.88"),
            ip("255.255.255.0")
        ));
        assert!(same_network(
            ip("fd00:1::5"),
            ip("fd00:1::1"),
            ip("ffff:ffff:ffff:ffff::")
        ));
        assert!(!same_network(ip("10.0.0.1"), ip("10.0.0.2"), ip("0.0.0.0")));
        assert!(!same_network(
            ip("10.0.0.1"),
            ip("fd00::1"),
            ip("255.0.0.0")
        ));
    }

    #[test]
    fn what_counts_as_local() {
        assert!(is_local(ip("169.254.10.1")));
        assert!(is_local(ip("fe80::1")));
        assert!(is_local(ip("224.0.0.251")));
        assert!(!is_local(ip("127.0.0.1")));
        assert!(!is_local(ip("::1")));
        // Each broadcast-capable interface's own network is.
        for (addr, _) in local_networks() {
            assert!(is_local(addr), "{addr}");
        }
    }

    #[tokio::test]
    async fn trying_a_closed_port_is_refused_and_an_open_one_connects() {
        let open = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = open.local_addr().unwrap().port();
        let t = try_addresses(&Target {
            host: "127.0.0.1".into(),
            port,
        })
        .await;
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].answer, Answer::Connected);
        assert!(!t[0].local);
        drop(open);
        let t = try_addresses(&Target {
            host: "127.0.0.1".into(),
            port,
        })
        .await;
        assert_eq!(t[0].answer, Answer::Refused);
    }
}
