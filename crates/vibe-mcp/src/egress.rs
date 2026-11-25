//! Posting to a URL a client named, for webhook delivery (PRD: MCP interface,
//! Events), without being turned on this machine or its network (SSRF).
//!
//! HTTPS, to a public address; or, so a receiver on this machine or the
//! tailnet works, HTTP or HTTPS to loopback or a Tailscale address
//! (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`, which is what `*.ts.net` names
//! resolve to). Anything else that isn't public is refused: private ranges,
//! link-local (where cloud metadata answers), unique-local, multicast and
//! reserved ones, and an IPv6 address that only wraps one of those.
//!
//! A name is resolved for every request, each address it gives is checked,
//! and the connection goes to one that passed, with the name kept for TLS
//! and `Host`: so a name that resolves to something else the next time
//! (DNS rebinding) is checked again. Redirects are never followed.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use bytes::Bytes;
use http::{Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;

/// The most of an answer's body that's read: a verification's echo is small.
const MAX_ANSWER: usize = 64 << 10;

/// The longest URL taken.
pub const MAX_URL: usize = 2048;

/// Which addresses that aren't public may be posted to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// This machine.
    pub loopback: bool,
    /// The tailnet.
    pub tailscale: bool,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            loopback: true,
            tailscale: true,
        }
    }
}

/// Why a post didn't get an answer, or the answer it got: each as one of
/// the categories a client is told, never what the other end said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// Nothing to connect to that may be, or nothing took the connection.
    Refused,
    Timeout,
    Tls,
    /// An answer that isn't 2xx.
    Status(u16),
}

impl Failure {
    /// The category: `connection_refused`, `timeout`, `tls_error`,
    /// `http_4xx` or `http_5xx`. Any status that's not 5xx is `http_4xx`, a
    /// redirect too: the receiver didn't take it.
    pub fn reason(self) -> &'static str {
        match self {
            Failure::Refused => "connection_refused",
            Failure::Timeout => "timeout",
            Failure::Tls => "tls_error",
            Failure::Status(s) if s >= 500 => "http_5xx",
            Failure::Status(_) => "http_4xx",
        }
    }
}

/// A URL that may be posted to, as far as can be told without resolving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub https: bool,
    /// Without brackets.
    pub host: String,
    pub port: u16,
    /// As `Host` says it.
    pub authority: String,
    pub path: String,
}

fn ipv4_in(ip: Ipv4Addr, net: [u8; 4], bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u32::MAX << (32 - bits)
    };
    u32::from(ip) & mask == u32::from(Ipv4Addr::from(net)) & mask
}

fn ipv6_in(ip: Ipv6Addr, net: Ipv6Addr, bits: u32) -> bool {
    let mask = if bits == 0 {
        0
    } else {
        u128::MAX << (128 - bits)
    };
    u128::from(ip) & mask == u128::from(net) & mask
}

/// The IPv4 address an IPv6 one only wraps: mapped (`::ffff:a.b.c.d`),
/// NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`).
fn wrapped(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    if let Some(v4) = ip.to_ipv4_mapped() {
        return Some(v4);
    }
    let o = ip.octets();
    if ipv6_in(ip, Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96) {
        return Some(Ipv4Addr::new(o[12], o[13], o[14], o[15]));
    }
    if ipv6_in(ip, Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16) {
        return Some(Ipv4Addr::new(o[2], o[3], o[4], o[5]));
    }
    None
}

/// Tailscale's: `100.64.0.0/10` and `fd7a:115c:a1e0::/48`, less
/// `100.100.100.0/24`, its own service addresses (and a cloud's metadata
/// service, on one).
fn is_tailscale(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ipv4_in(v4, [100, 64, 0, 0], 10) && !ipv4_in(v4, [100, 100, 100, 0], 24),
        IpAddr::V6(v6) => ipv6_in(v6, Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0), 48),
    }
}

/// Whether it's reachable on the internet: not in any of IANA's special
/// ranges that aren't.
fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => ![
            ([0, 0, 0, 0], 8),
            ([10, 0, 0, 0], 8),
            ([100, 64, 0, 0], 10),
            ([127, 0, 0, 0], 8),
            ([169, 254, 0, 0], 16),
            ([172, 16, 0, 0], 12),
            ([192, 0, 0, 0], 24),
            ([192, 0, 2, 0], 24),
            ([192, 88, 99, 0], 24),
            ([192, 168, 0, 0], 16),
            ([198, 18, 0, 0], 15),
            ([198, 51, 100, 0], 24),
            ([203, 0, 113, 0], 24),
            ([224, 0, 0, 0], 4),
            ([240, 0, 0, 0], 4),
        ]
        .iter()
        .any(|(net, bits)| ipv4_in(v4, *net, *bits)),
        // Global unicast is 2000::/3; less what's reserved in it.
        IpAddr::V6(v6) => {
            ipv6_in(v6, Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3)
                && ![
                    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
                    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
                    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
                ]
                .iter()
                .any(|(net, bits)| ipv6_in(v6, *net, *bits))
        }
    }
}

/// Whether `ip` may be posted to, over HTTPS or not.
pub fn allowed(ip: IpAddr, https: bool, policy: Policy) -> bool {
    let ip = match ip {
        IpAddr::V6(v6) => wrapped(v6).map_or(ip, IpAddr::V4),
        v4 => v4,
    };
    if ip.is_loopback() {
        return policy.loopback;
    }
    if is_tailscale(ip) {
        return policy.tailscale;
    }
    https && is_public(ip)
}

/// Checks `url` as far as it can be without resolving it: HTTPS, or HTTP
/// to loopback or the tailnet; no credentials in it; and, if its host is an
/// address, one [`allowed`].
pub fn check_url(url: &str, policy: Policy) -> Result<Target, String> {
    if url.len() > MAX_URL {
        return Err(format!("the URL is longer than {MAX_URL} characters"));
    }
    let uri: Uri = url.parse().map_err(|_| "it isn't a URL".to_owned())?;
    let https = match uri.scheme_str() {
        Some("https") => true,
        Some("http") => false,
        _ => return Err("it must be an https URL".into()),
    };
    let authority = uri
        .authority()
        .ok_or_else(|| "it has no host".to_owned())?
        .as_str()
        .to_owned();
    if authority.contains('@') {
        return Err("it can't hold a user name or password".into());
    }
    let host = uri.host().unwrap_or_default();
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
        .to_ascii_lowercase();
    if host.is_empty() {
        return Err("it has no host".into());
    }
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !allowed(ip, https, policy) {
            return Err(if https {
                format!("{ip} isn't a public address")
            } else {
                "it must be an https URL: http is only for this machine and the tailnet".into()
            });
        }
    } else if !https {
        let fine = (policy.loopback && host == "localhost")
            || (policy.tailscale && host.ends_with(".ts.net"));
        if !fine {
            return Err(
                "it must be an https URL: http is only for this machine and the tailnet".into(),
            );
        }
    }
    let path = uri
        .path_and_query()
        .map_or_else(|| "/".to_owned(), |p| p.as_str().to_owned());
    Ok(Target {
        https,
        host,
        port,
        authority,
        path,
    })
}

/// An answer: its status, and up to [`MAX_ANSWER`] of its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: u16,
    pub body: Vec<u8>,
}

/// The TLS client: this machine's roots.
fn tls() -> Arc<rustls::ClientConfig> {
    static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    Arc::clone(TLS.get_or_init(|| {
        let mut roots = rustls::RootCertStore::empty();
        // What can't be read just isn't trusted: a post then fails as
        // `tls_error`.
        let _ = roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        let provider = rustls::crypto::ring::default_provider();
        Arc::new(
            rustls::ClientConfig::builder_with_provider(provider.into())
                .with_safe_default_protocol_versions()
                .expect("ring supports the default protocol versions")
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    }))
}

/// The addresses `target` may be connected to now.
async fn addresses(target: &Target, policy: Policy) -> Result<Vec<SocketAddr>, Failure> {
    let all: Vec<SocketAddr> = match target.host.parse::<IpAddr>() {
        Ok(ip) => vec![SocketAddr::new(ip, target.port)],
        Err(_) => tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|_| Failure::Refused)?
            .collect(),
    };
    let fine: Vec<SocketAddr> = all
        .into_iter()
        .filter(|a| allowed(a.ip(), target.https, policy))
        .collect();
    if fine.is_empty() {
        return Err(Failure::Refused);
    }
    Ok(fine)
}

async fn exchange<S>(
    io: S,
    target: &Target,
    headers: &[(&str, String)],
    body: Bytes,
) -> Result<Answer, Failure>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .map_err(|_| Failure::Refused)?;
    let conn = tokio::spawn(conn);
    let mut request = Request::post(target.path.as_str())
        .header(http::header::HOST, target.authority.as_str())
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(
            http::header::USER_AGENT,
            concat!("vibe-os-native/", env!("CARGO_PKG_VERSION")),
        );
    for (name, value) in headers {
        request = request.header(*name, value.as_str());
    }
    let request = request
        .body(Full::new(body))
        .map_err(|_| Failure::Refused)?;
    let reply = sender
        .send_request(request)
        .await
        .map_err(|_| Failure::Refused)?;
    let status = reply.status().as_u16();
    let mut body = reply.into_body();
    let mut out = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(frame) = frame else { break };
        if let Ok(data) = frame.into_data() {
            let room = MAX_ANSWER.saturating_sub(out.len());
            out.extend_from_slice(&data[..data.len().min(room)]);
            if out.len() >= MAX_ANSWER {
                break;
            }
        }
    }
    conn.abort();
    Ok(Answer { status, body: out })
}

/// Posts `body` to `url`, within `within` all told, if [`check_url`] and
/// [`allowed`] let it.
pub async fn post(
    url: &str,
    policy: Policy,
    headers: &[(&str, String)],
    body: Vec<u8>,
    within: Duration,
) -> Result<Answer, Failure> {
    let target = check_url(url, policy).map_err(|_| Failure::Refused)?;
    let go = async {
        let mut tcp = None;
        for addr in addresses(&target, policy).await? {
            if let Ok(s) = TcpStream::connect(addr).await {
                tcp = Some(s);
                break;
            }
        }
        let tcp = tcp.ok_or(Failure::Refused)?;
        let body = Bytes::from(body);
        if target.https {
            let name = rustls::pki_types::ServerName::try_from(target.host.clone())
                .map_err(|_| Failure::Tls)?;
            let tls = tokio_rustls::TlsConnector::from(tls())
                .connect(name, tcp)
                .await
                .map_err(|_| Failure::Tls)?;
            exchange(tls, &target, headers, body).await
        } else {
            exchange(tcp, &target, headers, body).await
        }
    };
    tokio::time::timeout(within, go)
        .await
        .unwrap_or(Err(Failure::Timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(ip: &str, https: bool) -> bool {
        allowed(ip.parse().unwrap(), https, Policy::default())
    }

    #[test]
    fn public_addresses_over_https_and_the_tailnet_and_loopback_over_either() {
        for ip in ["93.184.216.34", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(ok(ip, true), "{ip}");
            assert!(!ok(ip, false), "{ip} over http");
        }
        for ip in [
            "127.0.0.1",
            "127.8.9.1",
            "::1",
            "::ffff:127.0.0.1",
            "100.64.0.1",
            "100.101.102.103",
            "100.127.255.254",
            "fd7a:115c:a1e0::1",
            "fd7a:115c:a1e0:ab12::42",
        ] {
            assert!(ok(ip, true) && ok(ip, false), "{ip}");
        }
    }

    #[test]
    fn nothing_else_that_isnt_public() {
        for ip in [
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.100.100.100",
            "100.100.100.200",
            "192.0.2.1",
            "192.0.0.170",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "203.0.113.9",
            "::",
            "fe80::1",
            "fc00::1",
            "fd00:ec2::254",
            "fd7a:115c:a1e1::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            // Private ones wrapped in IPv6.
            "::ffff:10.0.0.1",
            "::ffff:169.254.169.254",
            "64:ff9b::a00:1",
            "2002:a00:1::",
            "2002:a9fe:a9fe::",
        ] {
            assert!(!ok(ip, true), "{ip}");
            assert!(!ok(ip, false), "{ip} over http");
        }
        // A wrapped public one is as public.
        assert!(ok("::ffff:1.1.1.1", true) && ok("64:ff9b::101:101", true));
        assert!(ok("100.128.0.1", true), "just past the tailnet's range");
    }

    #[test]
    fn the_exceptions_can_be_turned_off() {
        let none = Policy {
            loopback: false,
            tailscale: false,
        };
        for ip in ["127.0.0.1", "::1", "100.64.0.1", "fd7a:115c:a1e0::1"] {
            assert!(!allowed(ip.parse().unwrap(), true, none), "{ip}");
        }
        assert!(check_url("http://localhost:9/x", none).is_err());
        assert!(check_url("http://box.tail1234.ts.net/x", none).is_err());
    }

    #[test]
    fn urls_are_https_unless_local_or_on_the_tailnet() {
        let p = Policy::default();
        let t = check_url("https://Receiver.example.com/mcp-events/cb_1?x=1", p).unwrap();
        assert_eq!(
            (t.https, t.host.as_str(), t.port, t.path.as_str()),
            (true, "receiver.example.com", 443, "/mcp-events/cb_1?x=1")
        );
        let t = check_url("http://[::1]:8080/hook", p).unwrap();
        assert_eq!(
            (t.https, t.host.as_str(), t.port, t.authority.as_str()),
            (false, "::1", 8080, "[::1]:8080")
        );
        for fine in [
            "http://127.0.0.1:9/x",
            "http://localhost/x",
            "http://box.tail1234.ts.net/hook",
            "http://100.70.1.2:8000/",
            "https://box.tail1234.ts.net/hook",
            "https://8.8.8.8/x",
        ] {
            assert!(check_url(fine, p).is_ok(), "{fine}");
        }
        for bad in [
            "http://receiver.example.com/x",
            "http://10.0.0.1/x",
            "https://10.0.0.1/x",
            "https://169.254.169.254/latest/meta-data",
            "https://[fe80::1]/x",
            "https://user:pass@receiver.example.com/x",
            "ftp://receiver.example.com/x",
            "file:///etc/passwd",
            "https://",
            "/relative",
            "not a url",
            "",
        ] {
            assert!(check_url(bad, p).is_err(), "{bad}");
        }
        assert!(check_url(&format!("https://a.example/{}", "x".repeat(MAX_URL)), p).is_err());
    }

    #[test]
    fn categories_say_no_more_than_the_kind() {
        assert_eq!(Failure::Status(404).reason(), "http_4xx");
        assert_eq!(Failure::Status(302).reason(), "http_4xx");
        assert_eq!(Failure::Status(503).reason(), "http_5xx");
        assert_eq!(Failure::Refused.reason(), "connection_refused");
        assert_eq!(Failure::Timeout.reason(), "timeout");
        assert_eq!(Failure::Tls.reason(), "tls_error");
    }

    #[tokio::test]
    async fn a_name_that_resolves_to_nothing_allowed_is_refused() {
        // localhost resolves to loopback, which this policy doesn't allow.
        let none = Policy {
            loopback: false,
            tailscale: false,
        };
        let target = Target {
            https: true,
            host: "localhost".into(),
            port: 9,
            authority: "localhost:9".into(),
            path: "/".into(),
        };
        assert_eq!(addresses(&target, none).await, Err(Failure::Refused));
    }
}
