//! MCP served publicly, as the Linux host that's the external address
//! serves it (PRD: External access): `https://<domain>/mcp`, with the same
//! key gate and tools as on loopback, behind TLS. Every call is
//! rate-limited and logged: by the client's address before its key is
//! looked at, and harder for a client that keeps sending keys that aren't
//! any.
//!
//! The certificate comes from the [`ResolvesServerCert`] it's given, which
//! also answers Let's Encrypt's TLS-ALPN-01 challenges: a connection that
//! negotiates `acme-tls/1` is closed once its handshake is done, as the
//! challenge needs nothing more.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use http::{Request, Response, StatusCode, header};
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::ServerConfig;
use rustls::server::ResolvesServerCert;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower_service::Service as _;

use crate::http::{
    Body, HEADER_TIMEOUT, MAX_BODY, READ_TIMEOUT, bearer, plain, read_whole, service, unauthorized,
    verify,
};
use crate::{Daemon, PATH};

/// The ALPN protocol of Let's Encrypt's TLS-ALPN-01 challenge (RFC 8737).
pub const ACME_TLS_ALPN: &[u8] = b"acme-tls/1";

/// How long a client has to finish its TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The most connections open at once. A refused request closes its
/// connection, so only clients with a key hold one for long.
const MAX_CONNECTIONS: usize = 1024;

/// The TLS setup the public endpoint serves with: TLS 1.2 and 1.3, HTTP/1.1,
/// and `acme-tls/1` for Let's Encrypt, with certificates from `certs`.
pub fn server_config(certs: Arc<dyn ResolvesServerCert>) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports TLS 1.2 and 1.3")
        .with_no_client_auth()
        .with_cert_resolver(certs);
    config.alpn_protocols = vec![b"http/1.1".to_vec(), ACME_TLS_ALPN.to_vec()];
    Arc::new(config)
}

// Rate limits.

/// How hard the public endpoint is on one client: its address, or its IPv6
/// /64, which one machine usually has whole.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Limits {
    /// Calls a client may make at once, before it must keep to `per_second`.
    pub burst: f64,
    pub per_second: f64,
    /// Calls with a key that isn't one (or none) a client may make, before
    /// all its calls are refused; it gets another every `refused_every`.
    pub refused_keys: f64,
    pub refused_every: Duration,
    /// Connections a client may have open at once.
    pub connections: usize,
    /// Clients kept track of at once; past that, idle ones are forgotten, and
    /// a new one waits if none is.
    pub clients: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // A wait polls through its own call, so even a busy
            // orchestrator makes a few calls a second.
            burst: 60.0,
            per_second: 10.0,
            refused_keys: 10.0,
            refused_every: Duration::from_secs(60),
            connections: 16,
            clients: 10_000,
        }
    }
}

/// A token bucket.
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(capacity: f64, now: Instant) -> Bucket {
        Bucket {
            tokens: capacity,
            at: now,
        }
    }

    fn refill(&mut self, now: Instant, capacity: f64, per_second: f64) {
        let gained = now.saturating_duration_since(self.at).as_secs_f64() * per_second;
        self.tokens = (self.tokens + gained).min(capacity);
        self.at = self.at.max(now);
    }

    /// How long until it has a token, if it has none now.
    fn wait(&mut self, now: Instant, capacity: f64, per_second: f64) -> Option<Duration> {
        self.refill(now, capacity, per_second);
        (self.tokens < 1.0).then(|| Duration::from_secs_f64((1.0 - self.tokens) / per_second))
    }

    fn take(&mut self, now: Instant, capacity: f64, per_second: f64) -> Result<(), Duration> {
        if let Some(wait) = self.wait(now, capacity, per_second) {
            return Err(wait);
        }
        self.tokens -= 1.0;
        Ok(())
    }
}

#[derive(Debug)]
struct Client {
    calls: Bucket,
    refused: Bucket,
    open: usize,
    /// When it was last heard from.
    seen: Instant,
    /// When its refusal was last logged: once a minute is enough.
    told: Option<Instant>,
}

/// A call [`Limiter::admit`] turned away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refused {
    /// How long until it may call again.
    pub wait: Duration,
    /// Whether to log it: the first time, then once a minute while it lasts.
    pub tell: bool,
}

/// The public endpoint's rate limits, per client.
#[derive(Debug)]
pub struct Limiter {
    limits: Limits,
    clients: Mutex<HashMap<IpAddr, Client>>,
}

/// Who a limit counts against: an IPv4 address (one mapped into IPv6
/// included), or an IPv6 address's /64.
pub fn client_of(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let s = v6.segments();
                IpAddr::V6(Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0))
            }
        },
        v4 => v4,
    }
}

/// A connection counted against its client, until it's dropped.
#[derive(Debug)]
pub struct Open {
    limiter: Arc<Limiter>,
    client: IpAddr,
}

impl Drop for Open {
    fn drop(&mut self) {
        if let Some(c) = self.limiter.lock().get_mut(&self.client) {
            c.open = c.open.saturating_sub(1);
        }
    }
}

const TELL_EVERY: Duration = Duration::from_secs(60);

impl Limiter {
    pub fn new(limits: Limits) -> Limiter {
        Limiter {
            limits,
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<IpAddr, Client>> {
        self.clients.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Runs `f` on `ip`'s client, made if it's new. With no room for a new
    /// one, `None`.
    fn with<T>(&self, ip: IpAddr, now: Instant, f: impl FnOnce(&mut Client) -> T) -> Option<T> {
        let l = self.limits;
        let mut clients = self.lock();
        let key = client_of(ip);
        if !clients.contains_key(&key) && clients.len() >= l.clients {
            // Forget those with nothing open and nothing held against them.
            clients.retain(|_, c| {
                c.calls.refill(now, l.burst, l.per_second);
                c.refused
                    .refill(now, l.refused_keys, 1.0 / l.refused_every.as_secs_f64());
                c.open > 0 || c.calls.tokens < l.burst || c.refused.tokens < l.refused_keys
            });
            // Still full, as when addresses are made up by the thousand:
            // the one heard from longest ago goes, rather than a newcomer
            // being turned away.
            if clients.len() >= l.clients {
                let oldest = clients
                    .iter()
                    .filter(|(_, c)| c.open == 0)
                    .min_by_key(|(_, c)| c.seen)
                    .map(|(ip, _)| *ip)?;
                clients.remove(&oldest);
            }
        }
        let c = clients.entry(key).or_insert_with(|| Client {
            calls: Bucket::full(l.burst, now),
            refused: Bucket::full(l.refused_keys, now),
            open: 0,
            seen: now,
            told: None,
        });
        c.seen = c.seen.max(now);
        Some(f(c))
    }

    /// A call from `ip` at `now`: let through, or refused, with how long it
    /// must wait. A client that has run out of refused keys is refused
    /// until it gets one back, whatever key it sends now.
    pub fn admit(&self, ip: IpAddr, now: Instant) -> Result<(), Refused> {
        let l = self.limits;
        let refused_rate = 1.0 / l.refused_every.as_secs_f64();
        self.with(ip, now, |c| {
            let blocked = c.refused.wait(now, l.refused_keys, refused_rate);
            let outcome = match blocked {
                Some(wait) => Err(wait),
                None => c.calls.take(now, l.burst, l.per_second),
            };
            outcome.map_err(|wait| {
                let tell = c
                    .told
                    .is_none_or(|t| now.saturating_duration_since(t) >= TELL_EVERY);
                if tell {
                    c.told = Some(now);
                }
                Refused { wait, tell }
            })
        })
        .unwrap_or(Err(Refused {
            wait: Duration::from_secs(1),
            tell: false,
        }))
    }

    /// Counts a call from `ip` whose key wasn't one, or that had none.
    pub fn refused_key(&self, ip: IpAddr, now: Instant) {
        let l = self.limits;
        let rate = 1.0 / l.refused_every.as_secs_f64();
        self.with(ip, now, |c| {
            let _ = c.refused.take(now, l.refused_keys, rate);
        });
    }

    /// A new connection from `ip`, unless it has as many open as it may.
    pub fn connect(self: &Arc<Self>, ip: IpAddr, now: Instant) -> Option<Open> {
        let max = self.limits.connections;
        let room = self.with(ip, now, |c| {
            if c.open >= max {
                return false;
            }
            c.open += 1;
            true
        })?;
        room.then(|| Open {
            limiter: Arc::clone(self),
            client: client_of(ip),
        })
    }
}

// Serving.

/// Serves MCP over TLS on `listener` until the task is aborted, with
/// `tls`'s certificates, and `limiter`'s limits on every client.
pub async fn serve<D: Daemon>(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    daemon: Arc<D>,
    limiter: Arc<Limiter>,
) {
    let svc = service(Arc::clone(&daemon), false);
    let acceptor = TlsAcceptor::from(tls);
    let room = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut last_error: Option<String> = None;
    // Every connection is this task's: when it's aborted, as when the
    // external address is turned off, they all end with it.
    let mut connections = tokio::task::JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}
        let Ok(permit) = Arc::clone(&room).acquire_owned().await else {
            return;
        };
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                let why = e.to_string();
                if last_error.as_ref() != Some(&why) {
                    daemon.log(&format!("public: can't accept a connection: {why}"));
                    last_error = Some(why);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        last_error = None;
        // An IPv4 client of the dual-stack listener reads as itself.
        let ip = peer.ip().to_canonical();
        // One client can't hold every connection there is.
        let Some(open) = limiter.connect(ip, Instant::now()) else {
            continue;
        };
        let (svc, daemon, limiter, acceptor) = (
            svc.clone(),
            Arc::clone(&daemon),
            Arc::clone(&limiter),
            acceptor.clone(),
        );
        connections.spawn(async move {
            let _held = (permit, open);
            let Ok(Ok(tls)) =
                tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
            else {
                return;
            };
            // Let's Encrypt's challenge is answered by the handshake itself.
            if tls.get_ref().1.alpn_protocol() == Some(ACME_TLS_ALPN) {
                return;
            }
            let conn = hyper::service::service_fn(move |req| {
                let (svc, daemon, limiter) =
                    (svc.clone(), Arc::clone(&daemon), Arc::clone(&limiter));
                async move { Ok::<_, Infallible>(handle(req, ip, svc, daemon, &limiter).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_TIMEOUT)
                .serve_connection(TokioIo::new(tls), conn)
                .await;
        });
    }
}

/// `s` as it may go in a log line: short, and only characters a method or
/// tool name has, as it's the caller's to say.
fn loggable(s: &str) -> String {
    let mut out: String = s
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '/' | '.' | ':'))
        .take(64)
        .collect();
    if out.is_empty() {
        out.push('?');
    }
    out
}

/// Of a JSON-RPC message, only what the log names; the rest is skipped
/// without being kept.
#[derive(serde::Deserialize)]
struct Call {
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    params: Option<CallParams>,
}

#[derive(serde::Deserialize)]
struct CallParams {
    #[serde(default)]
    name: Option<Value>,
}

/// What a request's body calls, for the log: its JSON-RPC method, and for
/// `tools/call` the tool. Nothing else of it, as a prompt is the caller's.
pub fn describe_call(body: &[u8]) -> String {
    if let Ok(call) = serde_json::from_slice::<Call>(body) {
        let name = call
            .params
            .and_then(|p| p.name)
            .and_then(|n| n.as_str().map(loggable));
        return match call.method.as_deref() {
            Some("tools/call") => format!("tools/call {}", name.as_deref().unwrap_or("?")),
            Some(m) => loggable(m),
            None => "a response".into(),
        };
    }
    match serde_json::from_slice::<Vec<serde::de::IgnoredAny>>(body) {
        Ok(items) => format!("a batch of {}", items.len()),
        Err(_) => "something that isn't JSON-RPC".into(),
    }
}

/// Reads a refused request's body to its end, keeping none of it, so the
/// answer isn't lost to a reset (see [`read_whole`]); within the same
/// limits.
async fn drain(req: Request<Incoming>) {
    use http_body_util::BodyExt as _;
    let mut body = req.into_body();
    let read = async {
        let mut seen = 0usize;
        while let Some(Ok(frame)) = body.frame().await {
            if let Ok(data) = frame.into_data() {
                seen += data.len();
                if seen > MAX_BODY {
                    return;
                }
            }
        }
    };
    let _ = tokio::time::timeout(READ_TIMEOUT, read).await;
}

/// A refusal, after which the connection closes: a client without a key
/// doesn't get to keep one open.
fn closing(mut r: Response<Body>) -> Response<Body> {
    r.headers_mut().insert(
        header::CONNECTION,
        header::HeaderValue::from_static("close"),
    );
    r
}

fn too_many(wait: Duration) -> Response<Body> {
    let mut r = plain(
        StatusCode::TOO_MANY_REQUESTS,
        "too many calls from this address; wait and try again",
    );
    let secs = wait.as_secs_f64().ceil().max(1.0) as u64;
    if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
        r.headers_mut().insert(header::RETRY_AFTER, v);
    }
    r
}

async fn handle<D: Daemon>(
    req: Request<Incoming>,
    ip: IpAddr,
    svc: rmcp::transport::streamable_http_server::StreamableHttpService<
        crate::Server<D>,
        rmcp::transport::streamable_http_server::session::never::NeverSessionManager,
    >,
    daemon: Arc<D>,
    limiter: &Limiter,
) -> Response<Body> {
    let begun = Instant::now();
    if let Err(refused) = limiter.admit(ip, begun) {
        if refused.tell {
            daemon.log(&format!(
                "public: {ip} is over its rate limit; its calls are refused for now"
            ));
        }
        drain(req).await;
        return closing(too_many(refused.wait));
    }
    let path = loggable(req.uri().path());
    // The path and the key first, from the headers: a request without a key
    // never has its body kept or read for anything. Each refusal counts
    // against the client, as a wrong key does.
    if req.uri().path() != PATH {
        limiter.refused_key(ip, Instant::now());
        daemon.log(&format!("public: {ip} {path}: refused, no such path"));
        drain(req).await;
        return closing(plain(StatusCode::NOT_FOUND, "VibeOS serves MCP at /mcp"));
    }
    let Some(caller) = verify(&req, &daemon).await else {
        limiter.refused_key(ip, Instant::now());
        let why = if bearer(&req).is_some() {
            "a key that isn't one"
        } else {
            "no key"
        };
        daemon.log(&format!("public: {ip} {}: refused, {why}", req.method()));
        drain(req).await;
        return closing(unauthorized());
    };
    let mut req = match read_whole(req).await {
        Ok(r) => r,
        Err(refused) => {
            daemon.log(&format!(
                "public: {ip} key {:?}: refused, {}",
                caller.name(),
                refused.status()
            ));
            return closing(*refused);
        }
    };
    let call = {
        use http_body_util::BodyExt as _;
        let body = req.body().clone().collect().await;
        body.map(|b| describe_call(&b.to_bytes()))
            .unwrap_or_default()
    };
    let name = caller.name().to_owned();
    req.extensions_mut().insert(caller);
    let mut svc = svc;
    let response = match svc.call(req).await {
        Ok(r) => r,
        Err(never) => match never {},
    };
    daemon.log(&format!(
        "public: {ip} key {name:?} {call}: {} in {} ms",
        response.status().as_u16(),
        begun.elapsed().as_millis()
    ));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            burst: 3.0,
            per_second: 1.0,
            refused_keys: 2.0,
            refused_every: Duration::from_secs(10),
            connections: 2,
            clients: 3,
        }
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_burst_then_a_steady_rate() {
        let l = Limiter::new(limits());
        let t = Instant::now();
        for _ in 0..3 {
            assert!(l.admit(ip("203.0.113.7"), t).is_ok());
        }
        let r = l.admit(ip("203.0.113.7"), t).unwrap_err();
        assert!(r.tell, "the first refusal is logged");
        assert!(r.wait > Duration::ZERO && r.wait <= Duration::from_secs(1));
        // Told once a minute, not on every call.
        assert!(!l.admit(ip("203.0.113.7"), t).unwrap_err().tell);
        // Another client has its own allowance.
        assert!(l.admit(ip("198.51.100.1"), t).is_ok());
        // A second later, one more.
        let t = t + Duration::from_secs(1);
        assert!(l.admit(ip("203.0.113.7"), t).is_ok());
        assert!(l.admit(ip("203.0.113.7"), t).is_err());
        let t = t + TELL_EVERY;
        assert!(l.admit(ip("203.0.113.7"), t).is_ok());
    }

    #[test]
    fn refused_keys_lock_a_client_out_even_with_a_good_one() {
        let l = Limiter::new(limits());
        let t = Instant::now();
        let a = ip("203.0.113.7");
        l.refused_key(a, t);
        assert!(l.admit(a, t).is_ok(), "one bad key isn't a lockout");
        l.refused_key(a, t);
        let r = l.admit(a, t).unwrap_err();
        assert!(r.wait > Duration::from_secs(9), "{r:?}");
        // It gets one more try every 10 s.
        let t = t + Duration::from_secs(10);
        assert!(l.admit(a, t).is_ok());
        l.refused_key(a, t);
        assert!(l.admit(a, t).is_err());
        // Others aren't held to it.
        assert!(l.admit(ip("198.51.100.1"), t).is_ok());
    }

    #[test]
    fn ipv6_counts_by_its_64_and_mapped_v4_as_v4() {
        assert_eq!(client_of(ip("2001:db8:1:2:aaaa::1")), ip("2001:db8:1:2::"));
        assert_eq!(client_of(ip("::ffff:203.0.113.7")), ip("203.0.113.7"));
        let l = Limiter::new(limits());
        let t = Instant::now();
        for i in 0..3 {
            assert!(l.admit(ip(&format!("2001:db8:1:2::{i}")), t).is_ok());
        }
        assert!(
            l.admit(ip("2001:db8:1:2:ffff::9"), t).is_err(),
            "one /64 is one client"
        );
    }

    #[test]
    fn connections_per_client_are_capped() {
        let l = Arc::new(Limiter::new(limits()));
        let t = Instant::now();
        let a = ip("203.0.113.7");
        let one = l.connect(a, t).unwrap();
        let _two = l.connect(a, t).unwrap();
        assert!(l.connect(a, t).is_none());
        assert!(l.connect(ip("198.51.100.1"), t).is_some());
        drop(one);
        assert!(l.connect(a, t).is_some());
    }

    #[test]
    fn a_flood_of_new_addresses_doesnt_shut_out_newcomers() {
        let l = Arc::new(Limiter::new(limits()));
        let t = Instant::now();
        // Three clients each held to a refused key: none is idle.
        for (i, a) in ["10.0.0.1", "10.0.0.2", "10.0.0.3"].iter().enumerate() {
            l.refused_key(ip(a), t + Duration::from_millis(i as u64));
        }
        // A newcomer still gets in: the one heard from longest ago goes.
        assert!(
            l.admit(ip("192.0.2.9"), t + Duration::from_millis(5))
                .is_ok()
        );
        assert!(!l.lock().contains_key(&ip("10.0.0.1")));
        assert_eq!(l.lock().len(), 3);
    }

    #[test]
    fn idle_clients_are_forgotten_to_make_room() {
        let l = Arc::new(Limiter::new(limits()));
        let t = Instant::now();
        let busy = l.connect(ip("10.0.0.1"), t).unwrap();
        l.admit(ip("10.0.0.2"), t).unwrap();
        l.admit(ip("10.0.0.3"), t + Duration::from_millis(1))
            .unwrap();
        // Full: the newcomer takes the place of the one heard from longest
        // ago with nothing open, never one with a connection.
        assert!(
            l.admit(ip("10.0.0.4"), t + Duration::from_millis(2))
                .is_ok()
        );
        assert!(l.lock().contains_key(&ip("10.0.0.1")));
        assert!(!l.lock().contains_key(&ip("10.0.0.2")));
        // Later, those that have earned their calls back are forgotten first.
        let t = t + Duration::from_secs(5);
        assert!(l.admit(ip("10.0.0.5"), t).is_ok());
        assert_eq!(l.lock().len(), 2);
        drop(busy);
    }

    #[test]
    fn calls_are_described_without_their_contents() {
        let call = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vibe_send","arguments":{"text":"secret plans"}}}"#;
        assert_eq!(describe_call(call), "tools/call vibe_send");
        assert_eq!(
            describe_call(br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#),
            "initialize"
        );
        assert_eq!(describe_call(b"[{}, {}]"), "a batch of 2");
        // Big or deep, nothing of it is kept to describe it.
        let big = format!("[{}{{}}]", "{\"a\":0},".repeat(100_000));
        assert_eq!(describe_call(big.as_bytes()), "a batch of 100001");
        let deep = "[".repeat(10_000);
        assert_eq!(
            describe_call(deep.as_bytes()),
            "something that isn't JSON-RPC"
        );
        assert_eq!(describe_call(b"nope"), "something that isn't JSON-RPC");
        // A name can't carry a line of its own into the log.
        let sneaky =
            br#"{"method":"tools/call","params":{"name":"x\npublic: 1.2.3.4 key \"admin\""}}"#;
        assert_eq!(describe_call(sneaky), "tools/call xpublic:1.2.3.4keyadmin");
    }
}
