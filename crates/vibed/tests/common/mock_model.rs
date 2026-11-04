//! A mock model on loopback for real Pi, speaking the Anthropic Messages API
//! as agent-tail's harness-ci `mock-server` does: no provider, no key, no
//! network. It keeps every key it was shown, so a test can say whose were used.
//!
//! A prompt's last words say what it answers: `call linear` calls the shared
//! service's tool and then says what it returned, `slow` takes ten seconds over
//! it, and anything else is `Answered.`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

#[allow(dead_code)] // Not every test checks the key.
pub(crate) const MOCK_KEY: &str = "fake-mock-key";

#[derive(Clone, Default)]
pub(crate) struct MockModel {
    pub(crate) port: u16,
    /// The `x-api-key`, or `authorization`, of every request.
    pub(crate) keys: Arc<Mutex<Vec<String>>>,
}

impl MockModel {
    /// Starts it on `bind` (an address a guest can reach, for a VM).
    pub(crate) fn start(bind: &str) -> MockModel {
        let listener = TcpListener::bind((bind, 0)).unwrap();
        let mock = MockModel {
            port: listener.local_addr().unwrap().port(),
            ..MockModel::default()
        };
        let keys = Arc::clone(&mock.keys);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let keys = Arc::clone(&keys);
                std::thread::spawn(move || {
                    let _ = serve(stream, &keys);
                });
            }
        });
        mock
    }

    /// The `models.json` that makes it Pi's `mock/mock-model`, at `host`.
    pub(crate) fn models_json(&self, host: &str) -> String {
        json!({"providers": {"mock": {
            "name": "Mock",
            "baseUrl": format!("http://{host}:{}", self.port),
            "api": "anthropic-messages",
            "apiKey": MOCK_KEY,
            "models": [{
                "id": "mock-model",
                "name": "Mock Model",
                "reasoning": false,
                "input": ["text"],
                "cost": {"input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0},
                "contextWindow": 128000,
                "maxTokens": 8192
            }]
        }}})
        .to_string()
    }
}

fn serve(mut stream: TcpStream, keys: &Mutex<Vec<String>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.to_ascii_lowercase();
            if name == "content-length" {
                length = value.trim().parse().unwrap_or(0);
            }
            if name == "x-api-key" || name == "authorization" {
                keys.lock().unwrap().push(value.trim().to_owned());
            }
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let frames = reply(&body);
    if said_last(&body).contains("slow") {
        std::thread::sleep(Duration::from_secs(10));
    }
    let mut out = String::from(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\
         connection: close\r\n\r\n",
    );
    out.push_str(&frames);
    stream.write_all(out.as_bytes())
}

/// What the last user message said, and whether it holds a tool's result.
fn said_last(body: &Value) -> String {
    body["messages"]
        .as_array()
        .and_then(|m| m.last())
        .map_or(String::new(), |m| m["content"].to_string())
}

fn sse(name: &str, data: Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn reply(body: &Value) -> String {
    let last = said_last(body);
    let tool = last.contains("call linear") && !last.contains("tool_result");
    let usage = json!({"input_tokens": 1, "output_tokens": 1, "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0});
    let mut out = sse(
        "message_start",
        json!({"type": "message_start", "message": {
            "id": "msg_mock", "type": "message", "role": "assistant", "model": "mock-model",
            "content": [], "stop_reason": null, "stop_sequence": null, "usage": usage,
        }}),
    );
    let stop = if tool {
        out += &sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0, "content_block": {
                "type": "tool_use", "id": "toolu_mock", "name": "mcp__vibe_linear__list_issues", "input": {},
            }}),
        );
        out += &sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
        );
        "tool_use"
    } else {
        let text = if last.contains("tool_result") {
            let said = if last.contains("issues on /mcp") {
                "issues"
            } else {
                "nothing"
            };
            format!("The tool said {said}.")
        } else {
            "Answered.".to_owned()
        };
        out += &sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "text", "text": ""}}),
        );
        out += &sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "text_delta", "text": text}}),
        );
        "end_turn"
    };
    out += &sse(
        "content_block_stop",
        json!({"type": "content_block_stop", "index": 0}),
    );
    out += &sse(
        "message_delta",
        json!({"type": "message_delta", "delta": {"stop_reason": stop, "stop_sequence": null},
            "usage": {"output_tokens": 1}}),
    );
    out + &sse("message_stop", json!({"type": "message_stop"}))
}
