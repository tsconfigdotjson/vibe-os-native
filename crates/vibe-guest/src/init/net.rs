//! The network: loopback, and DHCP on the VM's NIC from
//! Virtualization.framework's NAT, which gives each MAC address the same
//! address every time, across a restore too. The base image has no DHCP
//! client, so this is one: just enough of RFC 2131 to get a lease and keep
//! it.
//!
//! Requests go out through a UDP socket bound to the NIC. Replies are read
//! from a packet socket, so they arrive whether the server broadcasts them
//! or sends them to the address it's offering, which the NIC doesn't have
//! yet; without a packet socket, the broadcast flag asks for broadcasts.

use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use vibe_guest::proto::Network;

use crate::init::log;
use crate::init::session::lock;
use crate::init::sys::check;

// Generic Linux socket ioctls (linux/sockios.h), the same on every arch.
const SIOCADDRT: libc::c_ulong = 0x890B;
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;
const SIOCSIFADDR: libc::c_ulong = 0x8916;
const SIOCSIFNETMASK: libc::c_ulong = 0x891C;
const RTF_UP: libc::c_ushort = 0x1;
const RTF_GATEWAY: libc::c_ushort = 0x2;

const CLIENT_PORT: u16 = 68;
const SERVER_PORT: u16 = 67;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

/// What the guest calls itself to the DHCP server.
const HOSTNAME: &str = "vibe";

/// `struct ifreq`: a name and a 24-byte union.
#[repr(C)]
struct IfReq {
    name: [libc::c_char; libc::IFNAMSIZ],
    data: [u8; 24],
}

/// The kernel's `struct rtentry`.
#[repr(C)]
struct RtEntry {
    rt_pad1: libc::c_ulong,
    rt_dst: libc::sockaddr,
    rt_gateway: libc::sockaddr,
    rt_genmask: libc::sockaddr,
    rt_flags: libc::c_ushort,
    rt_pad2: libc::c_short,
    rt_pad3: libc::c_ulong,
    rt_pad4: *mut libc::c_void,
    rt_metric: libc::c_short,
    rt_dev: *mut libc::c_char,
    rt_mtu: libc::c_ulong,
    rt_window: libc::c_ulong,
    rt_irtt: libc::c_ushort,
}

fn ifreq(name: &str) -> io::Result<IfReq> {
    let mut req = IfReq {
        name: [0; libc::IFNAMSIZ],
        data: [0; 24],
    };
    if name.len() >= libc::IFNAMSIZ {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "interface name too long",
        ));
    }
    for (d, s) in req.name.iter_mut().zip(name.bytes()) {
        *d = s as libc::c_char;
    }
    Ok(req)
}

fn sockaddr(ip: Ipv4Addr) -> libc::sockaddr {
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(ip).to_be(),
        },
        sin_zero: [0; 8],
    };
    // SAFETY: sockaddr_in and sockaddr are both 16 bytes, and any bytes
    // make a sockaddr.
    unsafe { std::mem::transmute(sin) }
}

fn inet_socket(kind: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a new socket, owned at once.
    let fd = check(unsafe { libc::socket(libc::AF_INET, kind | libc::SOCK_CLOEXEC, 0) })?;
    // SAFETY: as above.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ioctl(fd: RawFd, request: libc::c_ulong, arg: *mut libc::c_void) -> io::Result<()> {
    // SAFETY: the callers pass a struct of the size `request` expects.
    check(unsafe { libc::ioctl(fd, request as _, arg) }).map(drop)
}

/// Brings an interface up.
pub(crate) fn up(name: &str) -> io::Result<()> {
    let s = inet_socket(libc::SOCK_DGRAM)?;
    let mut req = ifreq(name)?;
    ioctl(s.as_raw_fd(), SIOCGIFFLAGS, (&raw mut req).cast())?;
    let flags = i16::from_ne_bytes([req.data[0], req.data[1]]) | libc::IFF_UP as i16;
    req.data[..2].copy_from_slice(&flags.to_ne_bytes());
    ioctl(s.as_raw_fd(), SIOCSIFFLAGS, (&raw mut req).cast())
}

fn set_address(name: &str, ip: Ipv4Addr, mask: Ipv4Addr) -> io::Result<()> {
    let s = inet_socket(libc::SOCK_DGRAM)?;
    for (request, value) in [(SIOCSIFADDR, ip), (SIOCSIFNETMASK, mask)] {
        let mut req = ifreq(name)?;
        let addr = sockaddr(value);
        // SAFETY: a sockaddr is 16 plain bytes, which fit in the union.
        let bytes: [u8; 16] = unsafe { std::mem::transmute(addr) };
        req.data[..16].copy_from_slice(&bytes);
        ioctl(s.as_raw_fd(), request, (&raw mut req).cast())?;
    }
    Ok(())
}

fn add_default_route(gateway: Ipv4Addr) -> io::Result<()> {
    let s = inet_socket(libc::SOCK_DGRAM)?;
    let mut rt = RtEntry {
        rt_pad1: 0,
        rt_dst: sockaddr(Ipv4Addr::UNSPECIFIED),
        rt_gateway: sockaddr(gateway),
        rt_genmask: sockaddr(Ipv4Addr::UNSPECIFIED),
        rt_flags: RTF_UP | RTF_GATEWAY,
        rt_pad2: 0,
        rt_pad3: 0,
        rt_pad4: std::ptr::null_mut(),
        rt_metric: 0,
        rt_dev: std::ptr::null_mut(),
        rt_mtu: 0,
        rt_window: 0,
        rt_irtt: 0,
    };
    match ioctl(s.as_raw_fd(), SIOCADDRT, (&raw mut rt).cast()) {
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        other => other,
    }
}

/// The VM's NIC: the first Ethernet interface, `eth0` if there is one.
fn find_interface(timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut names: Vec<String> = fs::read_dir("/sys/class/net")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| {
                n != "lo"
                    && fs::read_to_string(format!("/sys/class/net/{n}/type"))
                        .is_ok_and(|t| t.trim() == "1")
            })
            .collect();
        names.sort_by_key(|n| (n != "eth0", n.clone()));
        if let Some(name) = names.into_iter().next() {
            return Some(name);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn mac_of(name: &str) -> io::Result<[u8; 6]> {
    let text = fs::read_to_string(format!("/sys/class/net/{name}/address"))?;
    let mut mac = [0u8; 6];
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() != 6 {
        return Err(io::Error::other(format!("{name} has no MAC address")));
    }
    for (b, p) in mac.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16).map_err(|_| io::Error::other("a bad MAC address"))?;
    }
    Ok(mac)
}

/// Gets a lease on the NIC and keeps it, for ever, in a thread of its own.
/// What it has is published in `network`.
pub(crate) fn start(network: Arc<Mutex<Option<Network>>>) {
    let _ = std::thread::Builder::new()
        .name("dhcp".into())
        .spawn(move || {
            let Some(name) = find_interface(Duration::from_secs(10)) else {
                log!("no network interface; the guest is offline");
                return;
            };
            if let Err(e) = up(&name) {
                log!("can't bring {name} up: {e}");
                return;
            }
            let mut backoff = Duration::from_secs(1);
            let mut held: Option<Lease> = None;
            loop {
                let result = match &held {
                    Some(lease) => Client::open(&name).and_then(|c| c.renew(lease)),
                    None => Client::open(&name).and_then(|c| c.acquire()),
                };
                match result {
                    Ok(lease) => {
                        backoff = Duration::from_secs(1);
                        if held.as_ref().map(|l| (l.address, l.mask, l.router))
                            != Some((lease.address, lease.mask, lease.router))
                        {
                            if let Err(e) = configure(&name, &lease) {
                                log!("can't configure {name}: {e}");
                            }
                            log!(
                                "{name} has {}/{} from {} for {}s",
                                lease.address,
                                prefix_len(lease.mask),
                                lease.server,
                                lease.seconds
                            );
                            *lock(&network) = Some(Network {
                                interface: name.clone(),
                                address: format!("{}/{}", lease.address, prefix_len(lease.mask)),
                                gateway: lease.router.map(|r| r.to_string()),
                                dns: lease.dns.iter().map(ToString::to_string).collect(),
                            });
                        }
                        let renew_in = Duration::from_secs(u64::from(lease.renew_after()));
                        held = Some(lease);
                        wait_for_renewal(renew_in);
                    }
                    Err(e) => {
                        if held.take().is_some() {
                            log!("couldn't renew the lease on {name}: {e}; asking afresh");
                            continue;
                        }
                        log!("no DHCP lease on {name} yet: {e}");
                        std::thread::sleep(backoff);
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                    }
                }
            }
        });
}

/// Asked for by a thaw: time the lease's clock didn't see has passed.
static RENEW: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

/// Renews the lease now rather than at its renewal time.
pub(crate) fn renew_soon() {
    *lock(&RENEW.0) = true;
    RENEW.1.notify_all();
}

/// Sleeps until `timeout` or [`renew_soon`], whichever comes first.
fn wait_for_renewal(timeout: Duration) {
    let mut asked = lock(&RENEW.0);
    let deadline = Instant::now() + timeout;
    while !*asked {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        asked = RENEW
            .1
            .wait_timeout(asked, left)
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0;
    }
    *asked = false;
}

fn prefix_len(mask: Ipv4Addr) -> u32 {
    u32::from(mask).count_ones()
}

fn configure(name: &str, lease: &Lease) -> io::Result<()> {
    set_address(name, lease.address, lease.mask)?;
    if let Some(router) = lease.router {
        add_default_route(router)?;
    }
    if !lease.dns.is_empty() {
        let text: String = lease
            .dns
            .iter()
            .map(|d| format!("nameserver {d}\n"))
            .collect();
        // A link to a resolver that isn't running is replaced by a file.
        let path = std::path::Path::new("/etc/resolv.conf");
        if path.is_symlink() {
            let _ = fs::remove_file(path);
        }
        fs::write(path, text)?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Lease {
    address: Ipv4Addr,
    mask: Ipv4Addr,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    server: Ipv4Addr,
    seconds: u32,
    renew: Option<u32>,
}

impl Lease {
    /// T1: when to renew, by default half the lease.
    fn renew_after(&self) -> u32 {
        self.renew.unwrap_or(self.seconds / 2).max(30)
    }
}

/// The message types of option 53.
const DISCOVER: u8 = 1;
const OFFER: u8 = 2;
const REQUEST: u8 = 3;
const ACK: u8 = 5;
const NAK: u8 = 6;

/// A DHCP client message. `ciaddr` is set when renewing.
fn message(
    kind: u8,
    xid: u32,
    mac: [u8; 6],
    ciaddr: Option<Ipv4Addr>,
    requested: Option<Ipv4Addr>,
    server: Option<Ipv4Addr>,
) -> Vec<u8> {
    let mut m = vec![0u8; 240];
    m[0] = 1; // BOOTREQUEST
    m[1] = 1; // Ethernet
    m[2] = 6;
    m[4..8].copy_from_slice(&xid.to_be_bytes());
    // The broadcast flag: send replies to everyone, as the NIC has no
    // address yet.
    m[10] = 0x80;
    if let Some(ip) = ciaddr {
        m[12..16].copy_from_slice(&ip.octets());
    }
    m[28..34].copy_from_slice(&mac);
    m[236..240].copy_from_slice(&MAGIC);
    m.extend_from_slice(&[53, 1, kind]);
    let mut client_id = vec![61, 7, 1];
    client_id.extend_from_slice(&mac);
    m.extend_from_slice(&client_id);
    if let Some(ip) = requested {
        m.extend_from_slice(&[50, 4]);
        m.extend_from_slice(&ip.octets());
    }
    if let Some(ip) = server {
        m.extend_from_slice(&[54, 4]);
        m.extend_from_slice(&ip.octets());
    }
    m.extend_from_slice(&[12, HOSTNAME.len() as u8]);
    m.extend_from_slice(HOSTNAME.as_bytes());
    // Subnet mask, router, DNS, lease time, renewal time.
    m.extend_from_slice(&[55, 5, 1, 3, 6, 51, 58]);
    m.push(255);
    m
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Reply {
    kind: u8,
    xid: u32,
    chaddr: [u8; 6],
    yiaddr: Ipv4Addr,
    mask: Option<Ipv4Addr>,
    router: Option<Ipv4Addr>,
    dns: Vec<Ipv4Addr>,
    server: Option<Ipv4Addr>,
    seconds: Option<u32>,
    renew: Option<u32>,
}

fn parse_reply(m: &[u8]) -> Option<Reply> {
    if m.len() < 240 || m[0] != 2 || m[236..240] != MAGIC {
        return None;
    }
    let ip = |b: &[u8]| -> Option<Ipv4Addr> { Some(Ipv4Addr::from(<[u8; 4]>::try_from(b).ok()?)) };
    let mut reply = Reply {
        kind: 0,
        xid: u32::from_be_bytes(m[4..8].try_into().ok()?),
        chaddr: m[28..34].try_into().ok()?,
        yiaddr: ip(&m[16..20])?,
        mask: None,
        router: None,
        dns: Vec::new(),
        server: None,
        seconds: None,
        renew: None,
    };
    let mut i = 240;
    while i < m.len() {
        let code = m[i];
        if code == 0 {
            i += 1;
            continue;
        }
        if code == 255 {
            break;
        }
        let len = *m.get(i + 1)? as usize;
        let value = m.get(i + 2..i + 2 + len)?;
        let secs = || value.try_into().ok().map(u32::from_be_bytes);
        match code {
            53 if len == 1 => reply.kind = value[0],
            1 if len == 4 => reply.mask = ip(value),
            3 if len >= 4 => reply.router = ip(&value[..4]),
            6 => {
                reply.dns = value
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| Ipv4Addr::from(*c))
                    .collect()
            }
            54 if len == 4 => reply.server = ip(value),
            51 => reply.seconds = secs(),
            58 => reply.renew = secs(),
            _ => {}
        }
        i += 2 + len;
    }
    (reply.kind != 0).then_some(reply)
}

/// The UDP payload of an IPv4 packet to the DHCP client port.
fn udp_payload(packet: &[u8]) -> Option<&[u8]> {
    if packet.len() < 20 || packet[0] >> 4 != 4 || packet[9] != libc::IPPROTO_UDP as u8 {
        return None;
    }
    let ihl = usize::from(packet[0] & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([packet[2], packet[3]])).min(packet.len());
    let udp = packet.get(ihl..total)?;
    if udp.len() < 8 || u16::from_be_bytes([udp[2], udp[3]]) != CLIENT_PORT {
        return None;
    }
    let len = usize::from(u16::from_be_bytes([udp[4], udp[5]])).clamp(8, udp.len());
    Some(&udp[8..len])
}

struct Client {
    name: String,
    mac: [u8; 6],
    send: OwnedFd,
    /// The packet socket replies are read from, or `None` to read them
    /// from `send`.
    recv: Option<File>,
}

impl Client {
    fn open(name: &str) -> io::Result<Client> {
        let mac = mac_of(name)?;
        let send = inet_socket(libc::SOCK_DGRAM)?;
        let fd = send.as_raw_fd();
        let one: libc::c_int = 1;
        let cname = CString::new(name)?;
        // SAFETY: setsockopt and bind with values that outlive the calls,
        // and sizes that match them.
        unsafe {
            for opt in [libc::SO_REUSEADDR, libc::SO_BROADCAST] {
                check(libc::setsockopt(
                    fd,
                    libc::SOL_SOCKET,
                    opt,
                    (&raw const one).cast(),
                    size_of::<libc::c_int>() as libc::socklen_t,
                ))?;
            }
            check(libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                cname.as_ptr().cast(),
                cname.as_bytes_with_nul().len() as libc::socklen_t,
            ))?;
            let addr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: CLIENT_PORT.to_be(),
                sin_addr: libc::in_addr { s_addr: 0 },
                sin_zero: [0; 8],
            };
            check(libc::bind(
                fd,
                (&raw const addr).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            ))?;
        }
        let recv = packet_socket(name).ok();
        Ok(Client {
            name: name.to_owned(),
            mac,
            send,
            recv,
        })
    }

    fn send(&self, message: &[u8]) -> io::Result<()> {
        let to = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: SERVER_PORT.to_be(),
            sin_addr: libc::in_addr { s_addr: u32::MAX },
            sin_zero: [0; 8],
        };
        // SAFETY: the buffer and address outlive the call.
        let n = unsafe {
            libc::sendto(
                self.send.as_raw_fd(),
                message.as_ptr().cast(),
                message.len(),
                0,
                (&raw const to).cast(),
                size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The next reply to `xid` of one of `kinds`, within `timeout`.
    fn receive(&self, xid: u32, kinds: &[u8], timeout: Duration) -> io::Result<Reply> {
        let deadline = Instant::now() + timeout;
        let mut buf = vec![0u8; 2048];
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::ErrorKind::TimedOut.into());
            }
            let fd = self
                .recv
                .as_ref()
                .map_or(self.send.as_raw_fd(), AsRawFd::as_raw_fd);
            crate::init::session::set_timeouts(fd, Some(left.max(Duration::from_millis(1))), None)?;
            let got = match &self.recv {
                Some(packets) => (&*packets).read(&mut buf),
                // SAFETY: a plain read into a buffer we own.
                None => {
                    let n = unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
                    if n < 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(n as usize)
                    }
                }
            };
            let n = match got {
                Ok(n) => n,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            let payload = match &self.recv {
                Some(_) => udp_payload(&buf[..n]),
                None => Some(&buf[..n]),
            };
            if let Some(reply) = payload.and_then(parse_reply)
                && reply.xid == xid
                && reply.chaddr == self.mac
                && kinds.contains(&reply.kind)
            {
                return Ok(reply);
            }
        }
    }

    /// Sends `message` and waits for a reply, a few times over.
    fn exchange(&self, message: &[u8], xid: u32, kinds: &[u8]) -> io::Result<Reply> {
        let mut wait = Duration::from_secs(1);
        let mut last = io::Error::from(io::ErrorKind::TimedOut);
        for _ in 0..4 {
            self.send(message)?;
            match self.receive(xid, kinds, wait) {
                Ok(reply) => return Ok(reply),
                Err(e) => last = e,
            }
            wait *= 2;
        }
        Err(io::Error::new(
            last.kind(),
            format!("no answer from a DHCP server on {}: {last}", self.name),
        ))
    }

    fn acquire(&self) -> io::Result<Lease> {
        let xid = random_u32();
        let discover = message(DISCOVER, xid, self.mac, None, None, None);
        let offer = self.exchange(&discover, xid, &[OFFER])?;
        let server = offer
            .server
            .ok_or_else(|| io::Error::other("an offer without a server id"))?;
        let request = message(
            REQUEST,
            xid,
            self.mac,
            None,
            Some(offer.yiaddr),
            Some(server),
        );
        self.finish(self.exchange(&request, xid, &[ACK, NAK])?)
    }

    fn renew(&self, lease: &Lease) -> io::Result<Lease> {
        let xid = random_u32();
        let request = message(REQUEST, xid, self.mac, Some(lease.address), None, None);
        self.finish(self.exchange(&request, xid, &[ACK, NAK])?)
    }

    fn finish(&self, reply: Reply) -> io::Result<Lease> {
        if reply.kind == NAK {
            return Err(io::Error::other("the server refused the address"));
        }
        let mask = reply.mask.unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
        Ok(Lease {
            address: reply.yiaddr,
            mask,
            router: reply.router,
            dns: reply.dns,
            server: reply.server.unwrap_or(Ipv4Addr::UNSPECIFIED),
            seconds: reply.seconds.unwrap_or(3600),
            renew: reply.renew,
        })
    }
}

/// A socket that gets every IPv4 packet arriving on the NIC.
fn packet_socket(name: &str) -> io::Result<File> {
    let proto = (libc::ETH_P_IP as u16).to_be();
    let cname = CString::new(name)?;
    // SAFETY: a new socket, owned at once, and a bind with an address that
    // outlives the call.
    unsafe {
        let fd = check(libc::socket(
            libc::AF_PACKET,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC,
            libc::c_int::from(proto),
        ))?;
        let owned = OwnedFd::from_raw_fd(fd);
        let index = libc::if_nametoindex(cname.as_ptr());
        if index == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut addr: libc::sockaddr_ll = std::mem::zeroed();
        addr.sll_family = libc::AF_PACKET as u16;
        addr.sll_protocol = proto;
        addr.sll_ifindex = index as libc::c_int;
        check(libc::bind(
            fd,
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_ll>() as libc::socklen_t,
        ))?;
        Ok(File::from(owned))
    }
}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    if File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .is_err()
    {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        return t.subsec_nanos() ^ std::process::id();
    }
    u32::from_ne_bytes(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAC: [u8; 6] = [0x02, 0x5f, 0x3a, 0x11, 0x9c, 0xe4];

    /// A server's reply to `request`, as macOS's bootpd would send it.
    fn server_reply(request: &[u8], kind: u8, extra: &[u8]) -> Vec<u8> {
        let mut m = request[..240].to_vec();
        m[0] = 2;
        m[16..20].copy_from_slice(&[192, 168, 64, 5]);
        m.truncate(240);
        m.extend_from_slice(&[53, 1, kind, 54, 4, 192, 168, 64, 1]);
        m.extend_from_slice(&[1, 4, 255, 255, 255, 0, 3, 4, 192, 168, 64, 1]);
        m.extend_from_slice(&[6, 8, 192, 168, 64, 1, 8, 8, 8, 8, 51, 4, 0, 1, 81, 128]);
        m.extend_from_slice(extra);
        m.push(255);
        m
    }

    #[test]
    fn a_discover_says_who_is_asking() {
        let m = message(DISCOVER, 0xdead_beef, MAC, None, None, None);
        assert_eq!((m[0], m[1], m[2]), (1, 1, 6));
        assert_eq!(&m[4..8], &0xdead_beef_u32.to_be_bytes());
        assert_eq!(m[10], 0x80, "the broadcast flag");
        assert_eq!(&m[28..34], &MAC);
        assert_eq!(&m[240..243], &[53, 1, DISCOVER]);
        assert_eq!(*m.last().unwrap(), 255);
        let r = message(
            REQUEST,
            1,
            MAC,
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            None,
            None,
        );
        assert_eq!(&r[12..16], &[10, 0, 0, 2]);
        let r = message(
            REQUEST,
            1,
            MAC,
            None,
            Some(Ipv4Addr::new(10, 0, 0, 2)),
            Some(Ipv4Addr::new(10, 0, 0, 1)),
        );
        let opts = &r[240..];
        assert!(opts.windows(6).any(|w| w == [50, 4, 10, 0, 0, 2]));
        assert!(opts.windows(6).any(|w| w == [54, 4, 10, 0, 0, 1]));
    }

    #[test]
    fn replies_parse() {
        let request = message(DISCOVER, 7, MAC, None, None, None);
        let reply = parse_reply(&server_reply(&request, OFFER, &[0, 58, 4, 0, 0, 1, 0])).unwrap();
        assert_eq!(
            reply,
            Reply {
                kind: OFFER,
                xid: 7,
                chaddr: MAC,
                yiaddr: Ipv4Addr::new(192, 168, 64, 5),
                mask: Some(Ipv4Addr::new(255, 255, 255, 0)),
                router: Some(Ipv4Addr::new(192, 168, 64, 1)),
                dns: vec![Ipv4Addr::new(192, 168, 64, 1), Ipv4Addr::new(8, 8, 8, 8)],
                server: Some(Ipv4Addr::new(192, 168, 64, 1)),
                seconds: Some(86400),
                renew: Some(256),
            }
        );
        // Truncated or foreign packets are ignored, not trusted.
        let good = server_reply(&request, ACK, &[]);
        for n in [0, 100, 239, 241, good.len() - 3] {
            let _ = parse_reply(&good[..n]);
        }
        assert!(parse_reply(&request).is_none(), "a request isn't a reply");
        let mut bad = good.clone();
        bad[236] = 0;
        assert!(parse_reply(&bad).is_none());
        let mut overrun = good[..240].to_vec();
        overrun.extend_from_slice(&[53, 1, ACK, 6, 200, 1]);
        assert!(parse_reply(&overrun).is_none());
    }

    #[test]
    fn leases_renew_at_half_time_or_t1() {
        let mut lease = Lease {
            address: Ipv4Addr::new(192, 168, 64, 5),
            mask: Ipv4Addr::new(255, 255, 255, 0),
            router: None,
            dns: vec![],
            server: Ipv4Addr::new(192, 168, 64, 1),
            seconds: 86400,
            renew: None,
        };
        assert_eq!(lease.renew_after(), 43200);
        lease.renew = Some(100);
        assert_eq!(lease.renew_after(), 100);
        lease.renew = Some(0);
        assert_eq!(lease.renew_after(), 30, "never in a tight loop");
        assert_eq!(prefix_len(lease.mask), 24);
    }

    #[test]
    fn udp_payloads_come_out_of_ip_packets() {
        let dhcp = [1u8, 2, 3, 4];
        let mut p = vec![
            0x45, 0, 0, 0, 0, 0, 0, 0, 64, 17, 0, 0, 192, 168, 64, 1, 255, 255, 255, 255,
        ];
        p.extend_from_slice(&[0, 67, 0, 68, 0, 12, 0, 0]);
        p.extend_from_slice(&dhcp);
        let total = p.len() as u16;
        p[2..4].copy_from_slice(&total.to_be_bytes());
        assert_eq!(udp_payload(&p), Some(&dhcp[..]));
        let mut other_port = p.clone();
        other_port[23] = 69;
        assert_eq!(udp_payload(&other_port), None);
        let mut tcp = p.clone();
        tcp[9] = 6;
        assert_eq!(udp_payload(&tcp), None);
        assert_eq!(udp_payload(&p[..10]), None);
    }
}
