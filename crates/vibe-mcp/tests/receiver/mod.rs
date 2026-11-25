//! A webhook receiver on loopback, for tests: it answers verifications as it's told,
//! answers deliveries with the statuses it's given (200 once they run out), and keeps
//! everything posted to it. HTTPS, with a certificate for `127.0.0.1` from a test CA
//! that only tests trust (`vibe_mcp::egress::trust_for_tests`, which starting one
//! does), and HTTP/1.1 within, a connection per request, as `vibed` posts them.

// Each test crate that includes this (vibe-mcp, vibed, vibe-cli) uses a different part of it.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;

/// The test CA, and the certificate it issued for `127.0.0.1`, `::1` and
/// `localhost` with its key: for tests only, never trusted anywhere else.
const TEST_CA: &[u8] = include_bytes!("ca.der");
const CERT: &[u8] = include_bytes!("cert.der");
const KEY: &[u8] = include_bytes!("key.der");

fn acceptor() -> tokio_rustls::TlsAcceptor {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let provider = rustls::crypto::ring::default_provider();
    let config = rustls::ServerConfig::builder_with_provider(provider.into())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(CERT.to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(KEY.to_vec())),
        )
        .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// How a verification is answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Echo {
    /// With its challenge.
    Right,
    /// With another.
    Wrong,
    /// With this status, and nothing.
    Status(u16),
    /// Not at all, for a while.
    Hang,
}

/// One post.
#[derive(Debug, Clone)]
pub(crate) struct Post {
    /// Where it was posted, as the request line says.
    pub(crate) path: String,
    /// Lowercased names.
    pub(crate) headers: HashMap<String, String>,
    pub(crate) body: Vec<u8>,
    pub(crate) json: Value,
    pub(crate) status: u16,
}

impl Post {
    pub(crate) fn header(&self, name: &str) -> &str {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map_or("", String::as_str)
    }

    pub(crate) fn is_verification(&self) -> bool {
        self.json["type"] == "verification"
    }
}

#[derive(Debug)]
struct Inner {
    posts: Vec<Post>,
    statuses: VecDeque<u16>,
    echo: Echo,
}

#[derive(Debug, Clone)]
pub(crate) struct Receiver {
    pub(crate) addr: SocketAddr,
    inner: Arc<Mutex<Inner>>,
    heard: Arc<tokio::sync::Notify>,
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    let (mut n, mut bits) = (0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = B64.iter().position(|a| *a == c).expect("base64") as u32;
        n = (n << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((n >> bits) as u8);
            n &= (1 << bits) - 1;
        }
    }
    out
}

/// A fresh `whsec_` secret.
pub(crate) fn secret(seed: u8) -> String {
    format!("whsec_{}", base64(&[seed; 32]))
}

/// Whether `post` is signed with `secret`, as a Standard Webhooks receiver checks it:
/// any of the `v1,` signatures, over `id.timestamp.body`, with a timestamp within five
/// minutes.
pub(crate) fn signed_with(secret: &str, post: &Post) -> bool {
    let key = unbase64(secret.strip_prefix("whsec_").expect("whsec_"));
    let (id, ts) = (post.header("webhook-id"), post.header("webhook-timestamp"));
    let Ok(ts_n) = ts.parse::<i64>() else {
        return false;
    };
    if (vibe_core::time::now() - ts_n).abs() > 300 {
        return false;
    }
    let mac = vibe_core::keys::hmac_sha256(
        &key,
        &[id.as_bytes(), b".", ts.as_bytes(), b".", &post.body],
    );
    let want = format!("v1,{}", base64(&mac));
    post.header("webhook-signature")
        .split(' ')
        .any(|s| s == want)
}

async fn read_request<S: AsyncRead + Unpin>(
    s: &mut S,
) -> Option<(String, HashMap<String, String>, Vec<u8>)> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let end = loop {
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        let n = s.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    };
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let headers: HashMap<String, String> = head
        .split("\r\n")
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut body = raw[end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&buf[..n]);
    }
    Some((path, headers, body))
}

impl Receiver {
    pub(crate) async fn start() -> Receiver {
        vibe_mcp::egress::trust_for_tests(TEST_CA);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tls = acceptor();
        let addr = listener.local_addr().unwrap();
        let r = Receiver {
            addr,
            inner: Arc::new(Mutex::new(Inner {
                posts: Vec::new(),
                statuses: VecDeque::new(),
                echo: Echo::Right,
            })),
            heard: Arc::new(tokio::sync::Notify::new()),
        };
        let me = r.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = listener.accept().await {
                let (me, tls) = (me.clone(), tls.clone());
                tokio::spawn(async move {
                    if let Ok(s) = tls.accept(s).await {
                        me.serve(s).await;
                    }
                });
            }
        });
        r
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(self, mut s: S) {
        let Some((path, headers, body)) = read_request(&mut s).await else {
            return;
        };
        let json: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let verification = json["type"] == "verification";
        let (status, answer) = {
            let mut inner = self.inner.lock().unwrap();
            if verification {
                match inner.echo {
                    Echo::Right => (200, json!({"challenge": json["challenge"]}).to_string()),
                    Echo::Wrong => (200, json!({"challenge": "not-it"}).to_string()),
                    Echo::Status(s) => (s, String::new()),
                    Echo::Hang => (0, String::new()),
                }
            } else {
                (inner.statuses.pop_front().unwrap_or(200), String::new())
            }
        };
        self.inner.lock().unwrap().posts.push(Post {
            path,
            headers,
            body,
            json,
            status,
        });
        self.heard.notify_waiters();
        if status == 0 {
            tokio::time::sleep(Duration::from_secs(30)).await;
            return;
        }
        let reply = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
            answer.len()
        );
        let _ = s.write_all(reply.as_bytes()).await;
        let _ = s.shutdown().await;
    }

    /// Its URL, at `path`.
    pub(crate) fn url(&self, path: &str) -> String {
        format!("https://{}{path}", self.addr)
    }

    /// The statuses the next deliveries get, in order.
    pub(crate) fn answer(&self, statuses: &[u16]) {
        self.inner.lock().unwrap().statuses.extend(statuses);
    }

    pub(crate) fn echo(&self, echo: Echo) {
        self.inner.lock().unwrap().echo = echo;
    }

    pub(crate) fn posts(&self) -> Vec<Post> {
        self.inner.lock().unwrap().posts.clone()
    }

    pub(crate) fn verifications(&self) -> Vec<Post> {
        self.posts()
            .into_iter()
            .filter(Post::is_verification)
            .collect()
    }

    /// Every delivery of an event.
    pub(crate) fn events(&self) -> Vec<Post> {
        self.posts()
            .into_iter()
            .filter(|p| !p.is_verification())
            .collect()
    }

    /// Waits up to `within` for at least `n` deliveries of events.
    pub(crate) async fn wait_events(&self, n: usize, within: Duration) -> Vec<Post> {
        let end = tokio::time::Instant::now() + within;
        loop {
            let heard = self.heard.notified();
            tokio::pin!(heard);
            heard.as_mut().enable();
            let events = self.events();
            if events.len() >= n {
                return events;
            }
            if tokio::time::timeout_at(end, heard).await.is_err() {
                let events = self.events();
                assert!(
                    events.len() >= n,
                    "{} of {n} deliveries in {within:?}: {:?}",
                    events.len(),
                    events.iter().map(|p| &p.json).collect::<Vec<_>>()
                );
                return events;
            }
        }
    }

    /// Waits up to `within` for a delivery that `matches`.
    pub(crate) async fn wait_for(&self, within: Duration, matches: impl Fn(&Post) -> bool) -> Post {
        let end = tokio::time::Instant::now() + within;
        loop {
            let heard = self.heard.notified();
            tokio::pin!(heard);
            heard.as_mut().enable();
            if let Some(p) = self.events().into_iter().find(|p| matches(p)) {
                return p;
            }
            if tokio::time::timeout_at(end, heard).await.is_err() {
                panic!(
                    "no such delivery in {within:?}: {:?}",
                    self.events().iter().map(|p| &p.json).collect::<Vec<_>>()
                );
            }
        }
    }
}
