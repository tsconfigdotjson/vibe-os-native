//! A small MCP client over Streamable HTTP, for tests: plain HTTP/1.1 on a
//! TCP connection per request, as an MCP client sends them.

#![allow(dead_code, unreachable_pub)]

use std::net::SocketAddr;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What came back for one request.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
    /// The JSON-RPC messages in the body: the one of a JSON response, or
    /// every event's of an event stream, in order.
    pub messages: Vec<Value>,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The response: the message with an id that isn't a request.
    pub fn response(&self) -> &Value {
        self.messages
            .iter()
            .find(|m| m.get("method").is_none() && m.get("id").is_some())
            .unwrap_or_else(|| panic!("no response in {}", self.body))
    }

    pub fn progress(&self) -> Vec<&Value> {
        self.messages
            .iter()
            .filter(|m| m["method"] == "notifications/progress")
            .collect()
    }
}

fn dechunk(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = body;
    loop {
        let Some(i) = rest.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = std::str::from_utf8(&rest[..i])
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let n = usize::from_str_radix(size.trim(), 16).unwrap();
        if n == 0 {
            return out;
        }
        rest = &rest[i + 2..];
        out.extend_from_slice(&rest[..n]);
        rest = &rest[n + 2..];
    }
}

/// Posts `body` to `/mcp` at `addr` with `headers`, and reads the whole
/// answer.
pub async fn post_raw(addr: SocketAddr, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut head = format!(
        "POST {path} HTTP/1.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
        head.push_str(&format!("Host: {addr}\r\n"));
    }
    for (n, v) in headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    s.write_all(head.as_bytes()).await.unwrap();
    s.write_all(body.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a head");
    let head = String::from_utf8_lossy(&raw[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(n, v)| (n.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let mut body = raw[end + 4..].to_vec();
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    if chunked {
        body = dechunk(&body);
    }
    let body = String::from_utf8(body).unwrap();
    let sse = headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("content-type") && v.starts_with("text/event-stream"));
    let messages = if sse {
        let mut out = Vec::new();
        for event in body.split("\n\n") {
            let data: Vec<&str> = event
                .lines()
                .filter_map(|l| l.strip_prefix("data:"))
                .map(|d| d.strip_prefix(' ').unwrap_or(d))
                .collect();
            let data = data.join("\n");
            if !data.trim().is_empty() {
                out.push(serde_json::from_str(&data).unwrap());
            }
        }
        out
    } else if body.trim().is_empty() || status != 200 {
        Vec::new()
    } else {
        vec![serde_json::from_str(&body).unwrap_or_else(|e| panic!("{e}: {body}"))]
    };
    Reply {
        status,
        headers,
        body,
        messages,
    }
}

/// An MCP client with a key.
#[derive(Debug, Clone)]
pub struct Client {
    pub addr: SocketAddr,
    pub key: String,
    /// The protocol version `initialize` agreed, sent on every later call.
    pub protocol: Option<String>,
    next_id: u64,
}

impl Client {
    pub fn new(addr: SocketAddr, key: &str) -> Client {
        Client {
            addr,
            key: key.to_owned(),
            protocol: None,
            next_id: 1,
        }
    }

    pub async fn post(&mut self, message: &Value) -> Reply {
        let auth = format!("Bearer {}", self.key);
        let mut headers = vec![("Authorization", auth.as_str())];
        let protocol = self.protocol.clone();
        if let Some(p) = &protocol {
            headers.push(("MCP-Protocol-Version", p));
        }
        post_raw(self.addr, "/mcp", &headers, &message.to_string()).await
    }

    fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// `initialize` as a 2025-era client sends it, then `initialized`.
    pub async fn initialize(&mut self, version: &str) -> Value {
        let id = self.id();
        let r = self
            .post(
                &json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
                "protocolVersion": version, "capabilities": {},
                "clientInfo": {"name": "test", "version": "1"}}}),
            )
            .await;
        assert_eq!(r.status, 200, "{}", r.body);
        let result = r.response()["result"].clone();
        self.protocol = result["protocolVersion"].as_str().map(str::to_owned);
        let n = self
            .post(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        assert!(
            n.status == 202 || n.status == 200,
            "{} {}",
            n.status,
            n.body
        );
        result
    }

    pub async fn list_tools(&mut self) -> Vec<Value> {
        let id = self.id();
        let r = self
            .post(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": {}}))
            .await;
        r.response()["result"]["tools"].as_array().unwrap().clone()
    }

    /// Calls `tool`, with a progress token, and returns the whole reply.
    pub async fn call_raw(&mut self, tool: &str, args: Value) -> Reply {
        let id = self.id();
        self.post(
            &json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
            "name": tool, "arguments": args, "_meta": {"progressToken": format!("p{id}")}}}),
        )
        .await
    }

    /// Calls `tool` and returns its result, which must not be an error.
    pub async fn call(&mut self, tool: &str, args: Value) -> Value {
        let r = self.call_raw(tool, args).await;
        let result = r.response()["result"].clone();
        assert_ne!(result["isError"], true, "{tool}: {}", text(&result));
        result
    }

    /// Calls `tool` and returns its error's text, which it must be.
    pub async fn fails(&mut self, tool: &str, args: Value) -> String {
        let r = self.call_raw(tool, args).await;
        let result = r.response()["result"].clone();
        assert_eq!(result["isError"], true, "{tool} should fail: {}", r.body);
        text(&result)
    }
}

/// A tool result's text.
pub fn text(result: &Value) -> String {
    result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_owned()
}
