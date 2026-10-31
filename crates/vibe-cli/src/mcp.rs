//! `vibe mcp`: MCP over stdio, for clients that can only start a command
//! (PRD: MCP interface, Transport).
//!
//! It's a bridge to the daemon's own MCP endpoint, as VibeOS's is: each
//! message read from stdin is posted to `/mcp` with the API key, and what
//! comes back, a JSON response or an event stream with progress, is
//! written to stdout a message per line. So auth is the same as over HTTP:
//! no key, no MCP. A cancel from the client closes the request it names,
//! which cancels the call on the server.
//!
//! stdout is the protocol channel, so everything this says goes to stderr.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

/// How long connecting to the endpoint may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a response may go silent: a full-length `vibe_wait`, between
/// its progress, keep-alives every 15 s, so this only ends a call the
/// server went away on.
const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// An `http://host:port/path` URL, the only kind this speaks: the
/// endpoint is on loopback or the tailnet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Url {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) path: String,
}

impl Url {
    pub(crate) fn parse(s: &str) -> Result<Url, String> {
        let rest = s.strip_prefix("http://").ok_or_else(|| {
            format!("{s}: vibe mcp speaks plain http to vibed, as on http://127.0.0.1:7878/mcp")
        })?;
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/mcp"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => (
                h,
                p.parse::<u16>()
                    .map_err(|_| format!("{s}: {p} isn't a port"))?,
            ),
            _ => (authority, 80),
        };
        if host.is_empty() {
            return Err(format!("{s}: no host"));
        }
        Ok(Url {
            host: host
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
            port,
            path: path.to_owned(),
        })
    }

    fn authority(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// Requests in flight, by their JSON-RPC id, so a cancel can close one.
type InFlight = Arc<Mutex<HashMap<String, TcpStream>>>;

/// The bridge's state, shared by the threads that carry each message.
struct Bridge {
    url: Url,
    key: String,
    /// What `initialize` agreed, sent with every later request as the
    /// Streamable HTTP transport asks.
    protocol: Mutex<Option<String>>,
    out: Mutex<io::Stdout>,
    in_flight: InFlight,
    /// Requests the client cancelled, which get no response.
    cancelled: Mutex<std::collections::HashSet<String>>,
}

impl Bridge {
    fn write(&self, message: &Value) {
        let mut out = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(out, "{message}");
        let _ = out.flush();
    }

    /// Answers request `id` with an error, as the server would have.
    fn error(&self, id: &Value, message: String) {
        eprintln!("vibe mcp: {message}");
        self.write(
            &json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32603, "message": message}}),
        );
    }

    fn post(&self, message: Value) {
        let id = message
            .get("id")
            .filter(|_| message.get("method").is_some())
            .cloned();
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "notifications/cancelled" {
            if let Some(req) = message.pointer("/params/requestId") {
                lock(&self.cancelled).insert(req.to_string());
                let stream = lock(&self.in_flight).remove(&req.to_string());
                if let Some(s) = stream {
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
            return;
        }
        let is_initialize = method == "initialize";
        let outcome = self.exchange(&message, id.as_ref(), is_initialize);
        let cancelled = id
            .as_ref()
            .is_some_and(|id| lock(&self.cancelled).remove(&id.to_string()));
        match outcome {
            Ok(()) => {}
            // The client gave up on it, and expects nothing more.
            Err(_) if cancelled => {}
            Err(e) => match &id {
                Some(id) => self.error(id, e),
                None => eprintln!("vibe mcp: {e}"),
            },
        }
        if let Some(id) = &id {
            lock(&self.in_flight).remove(&id.to_string());
        }
    }

    /// Posts `message` and writes out what comes back.
    fn exchange(
        &self,
        message: &Value,
        id: Option<&Value>,
        initialize: bool,
    ) -> Result<(), String> {
        let unreachable =
            |e: io::Error| format!("can't reach vibed's MCP at {}: {e}", self.url_text());
        let addr = self
            .url
            .authority()
            .to_socket_addrs()
            .map_err(unreachable)?
            .next()
            .ok_or_else(|| format!("{} has no address", self.url.host))?;
        let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).map_err(unreachable)?;
        stream
            .set_read_timeout(Some(READ_TIMEOUT))
            .map_err(|e| e.to_string())?;
        if let Some(id) = id
            && let Ok(clone) = stream.try_clone()
        {
            lock(&self.in_flight).insert(id.to_string(), clone);
        }
        let body = message.to_string();
        let protocol = lock(&self.protocol).clone();
        let mut head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.url.path,
            self.url.authority(),
            self.key,
            body.len()
        );
        if let Some(p) = protocol.filter(|_| !initialize) {
            head.push_str(&format!("MCP-Protocol-Version: {p}\r\n"));
        }
        head.push_str("\r\n");
        stream
            .write_all(head.as_bytes())
            .and_then(|()| stream.write_all(body.as_bytes()))
            .map_err(|e| format!("sending to vibed: {e}"))?;
        let mut reader = BufReader::new(stream);
        let response =
            read_head(&mut reader).map_err(|e| format!("reading vibed's answer: {e}"))?;
        let mut body: Box<dyn BufRead> = match (response.chunked, response.length) {
            (true, _) => Box::new(BufReader::new(Chunked::new(reader))),
            (false, Some(n)) => Box::new(BufReader::new(reader.take(n))),
            (false, None) => Box::new(reader),
        };
        match response.status {
            200 => {}
            202 => return Ok(()),
            401 => {
                return Err(format!(
                    "vibed refused the API key ({}): make one in Settings → API keys or with vibe key create NAME, and pass it with --key or VIBE_API_KEY",
                    text_of(&mut body)
                ));
            }
            status => return Err(format!("vibed answered {status}: {}", text_of(&mut body))),
        }
        let mut seen = |v: Value| {
            if initialize
                && let Some(p) = v.pointer("/result/protocolVersion").and_then(Value::as_str)
            {
                *lock(&self.protocol) = Some(p.to_owned());
            }
            self.write(&v);
        };
        if response.event_stream {
            read_events(&mut body, |data| {
                match serde_json::from_str::<Value>(data) {
                    Ok(v) => seen(v),
                    Err(e) => eprintln!("vibe mcp: an event that isn't JSON: {e}"),
                }
            })
            .map_err(|e| format!("reading vibed's events: {e}"))?;
        } else {
            let mut text = String::new();
            body.read_to_string(&mut text)
                .map_err(|e| format!("reading vibed's answer: {e}"))?;
            if !text.trim().is_empty() {
                let v: Value = serde_json::from_str(&text)
                    .map_err(|e| format!("vibed's answer isn't JSON: {e}"))?;
                // A batch's answers go out one per line.
                match v {
                    Value::Array(items) => items.into_iter().for_each(&mut seen),
                    v => seen(v),
                }
            }
        }
        Ok(())
    }

    fn url_text(&self) -> String {
        format!("http://{}{}", self.url.authority(), self.url.path)
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn text_of(body: &mut dyn BufRead) -> String {
    let mut text = String::new();
    let _ = body.take(4096).read_to_string(&mut text);
    text.trim().to_owned()
}

/// What matters of a response's head.
#[derive(Debug, PartialEq, Eq)]
struct Head {
    status: u16,
    chunked: bool,
    length: Option<u64>,
    event_stream: bool,
}

fn read_head(r: &mut impl BufRead) -> io::Result<Head> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_owned());
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status = line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("no HTTP status line"))?;
    let mut head = Head {
        status,
        chunked: false,
        length: None,
        event_stream: false,
    };
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            return Err(bad("the headers ended early"));
        }
        let line = line.trim_end();
        if line.is_empty() {
            return Ok(head);
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" => head.chunked = value.to_ascii_lowercase().contains("chunked"),
            "content-length" => head.length = value.parse().ok(),
            "content-type" => head.event_stream = value.starts_with("text/event-stream"),
            _ => {}
        }
    }
}

/// A chunked body, read as the plain bytes it carries.
struct Chunked<R> {
    inner: R,
    left: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    fn new(inner: R) -> Chunked<R> {
        Chunked {
            inner,
            left: 0,
            done: false,
        }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            self.inner.read_line(&mut line)?;
            let size = line.trim().split(';').next().unwrap_or("");
            self.left = u64::from_str_radix(size, 16).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a bad chunk size {size:?}"),
                )
            })?;
            if self.left == 0 {
                self.done = true;
                return Ok(0);
            }
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.left -= n as u64;
        if self.left == 0 {
            let mut crlf = String::new();
            self.inner.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

/// Calls `each` with the data of every event in an event stream, as each
/// arrives. Comments, keep-alives and events with no data are skipped.
fn read_events(r: &mut dyn BufRead, mut each: impl FnMut(&str)) -> io::Result<()> {
    let mut data: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        let l = line.trim_end_matches(['\r', '\n']);
        if n == 0 || l.is_empty() {
            if !data.is_empty() {
                let joined = data.join("\n");
                if !joined.trim().is_empty() {
                    each(&joined);
                }
                data.clear();
            }
            if n == 0 {
                return Ok(());
            }
            continue;
        }
        if let Some(d) = l.strip_prefix("data:") {
            data.push(d.strip_prefix(' ').unwrap_or(d).to_owned());
        }
    }
}

/// Bridges stdin and stdout to the endpoint at `url` with `key`, until
/// stdin ends and every request in flight has been answered.
pub(crate) fn run(url: Url, key: String) -> Result<(), String> {
    if !vibe_core::keys::is_key(&key) {
        return Err("that isn't an API key: keys look like vk_ and 52 letters and digits".into());
    }
    eprintln!(
        "vibe mcp: bridging stdio to http://{}{}",
        url.authority(),
        url.path
    );
    let bridge = Arc::new(Bridge {
        url,
        key,
        protocol: Mutex::new(None),
        out: Mutex::new(io::stdout()),
        in_flight: Arc::new(Mutex::new(HashMap::new())),
        cancelled: Mutex::new(std::collections::HashSet::new()),
    });
    let stdin = io::stdin();
    let mut lines = stdin.lock();
    let mut line = String::new();
    let mut carrying: Vec<std::thread::JoinHandle<()>> = Vec::new();
    loop {
        line.clear();
        if lines.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            // What's in flight is answered first, as `vibe rpc` does.
            for t in carrying {
                let _ = t.join();
            }
            return Ok(());
        }
        carrying.retain(|t| !t.is_finished());
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(line.trim()) {
            Ok(v) => v,
            Err(e) => {
                bridge.write(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": format!("Parse error: {e}")}}));
                continue;
            }
        };
        // `initialize` first, alone, so every later request carries the
        // version it agreed.
        if message.get("method").and_then(Value::as_str) == Some("initialize") {
            bridge.post(message);
            continue;
        }
        let b = Arc::clone(&bridge);
        carrying.push(std::thread::spawn(move || b.post(message)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            Url::parse("http://127.0.0.1:7878/mcp").unwrap(),
            Url {
                host: "127.0.0.1".into(),
                port: 7878,
                path: "/mcp".into()
            }
        );
        let u = Url::parse("http://[fd7a:115c:a1e0::1]:9000").unwrap();
        assert_eq!(
            (u.host.as_str(), u.port, u.path.as_str()),
            ("fd7a:115c:a1e0::1", 9000, "/mcp")
        );
        assert_eq!(u.authority(), "[fd7a:115c:a1e0::1]:9000");
        assert_eq!(Url::parse("http://mac-mini/x").unwrap().port, 80);
        assert!(Url::parse("https://vps.example.com/mcp").is_err());
        assert!(Url::parse("http://h:port/mcp").is_err());
        assert!(Url::parse("http:///mcp").is_err());
    }

    #[test]
    fn heads() {
        let raw = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\nrest";
        let mut r = io::Cursor::new(raw.as_bytes());
        let h = read_head(&mut r).unwrap();
        assert_eq!(
            h,
            Head {
                status: 200,
                chunked: true,
                length: None,
                event_stream: true
            }
        );
        let mut r = io::Cursor::new(&b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 5\r\n\r\n"[..]);
        let h = read_head(&mut r).unwrap();
        assert_eq!((h.status, h.length, h.chunked), (401, Some(5), false));
        assert!(read_head(&mut io::Cursor::new(&b"junk"[..])).is_err());
    }

    #[test]
    fn chunked_bodies_and_events() {
        let pieces = [
            "data: 1\n",
            "\n: ping\n\nid: 3\ndata:\n\n",
            "data: {\"a\":\ndata: 2}\n",
            "\n",
        ];
        let mut body = String::new();
        for p in pieces {
            body.push_str(&format!("{:x};ext=1\r\n{p}\r\n", p.len()));
        }
        body.push_str("0\r\n\r\n");
        let mut r = BufReader::new(Chunked::new(io::Cursor::new(body.as_bytes())));
        let mut got = Vec::new();
        read_events(&mut r, |d| got.push(d.to_owned())).unwrap();
        assert_eq!(got, ["1", "{\"a\":\n2}"]);
        let mut bad = Chunked::new(io::Cursor::new(&b"zz\r\n"[..]));
        assert!(bad.read(&mut [0; 4]).is_err());
    }
}
