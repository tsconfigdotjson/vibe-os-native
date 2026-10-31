//! `vibe mcp`, the stdio bridge, and `vibe key`, against an in-process
//! `vibed` serving MCP on a free port.

// vibed's test helpers: the golden world, with the stand-in for Claude.
#[path = "../../vibed/tests/common/mod.rs"]
mod common;

use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vibe_core::rpc::{AgentStartParams, KeyCreateParams, MAX_MESSAGE_BYTES, ProjectAddParams};
use vibed::core::Core;
use vibed::server;

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

struct World {
    golden: common::Golden,
    core: Arc<Core>,
    addr: SocketAddr,
    key: String,
    socket: std::path::PathBuf,
    _guard: server::SocketGuard,
    rt: tokio::runtime::Runtime,
}

fn world() -> World {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let golden = common::Golden::new();
    let core = Arc::new(Core::open(golden.config()).unwrap());
    let project = golden.home.join("src/app");
    rt.block_on(core.add_project(ProjectAddParams {
        path: Some(project.to_str().unwrap().into()),
        ..Default::default()
    }))
    .unwrap();
    let key = core
        .create_key(KeyCreateParams {
            name: "bridge".into(),
        })
        .unwrap()
        .secret;
    let listener = rt
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    rt.spawn(vibe_mcp::http::serve(listener, Arc::clone(&core), true));
    let socket = golden.dir.path().join("run/vibed.sock");
    let (unix, guard) = server::bind(&socket).unwrap();
    let served = Arc::clone(&core);
    rt.spawn(async move {
        let unix = tokio::net::UnixListener::from_std(unix).unwrap();
        server::serve(unix, served, MAX_MESSAGE_BYTES, std::future::pending()).await;
    });
    World {
        golden,
        core,
        addr,
        key,
        socket,
        _guard: guard,
        rt,
    }
}

/// A running `vibe mcp`, its stdin, and its stdout's lines.
struct Bridge {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
}

impl Bridge {
    fn start(w: &World, key: Option<&str>) -> Bridge {
        let mut cmd = Command::new(VIBE);
        cmd.args(["mcp", "--url", &format!("http://{}/mcp", w.addr)])
            .env("HOME", &w.golden.home)
            .env_remove("VIBE_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(k) = key {
            cmd.env("VIBE_API_KEY", k);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        Bridge {
            stdin: child.stdin.take(),
            child,
            lines,
        }
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn next(&self, within: Duration) -> Option<Value> {
        self.lines
            .recv_timeout(within)
            .ok()
            .map(|l| serde_json::from_str(&l).unwrap_or_else(|e| panic!("{e}: {l}")))
    }

    /// The next message, which must come within 20 s.
    fn expect(&self) -> Value {
        self.next(Duration::from_secs(20))
            .expect("a message from vibe mcp")
    }

    fn initialize(&mut self) -> Value {
        self.send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "stdio-client", "version": "1"}}}),
        );
        let r = self.expect();
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        r
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn the_bridge_carries_mcp_over_stdio() {
    let w = world();
    let mut b = Bridge::start(&w, Some(&w.key));
    let init = b.initialize();
    assert_eq!(init["id"], 1);
    assert_eq!(
        init["result"]["serverInfo"]["name"], "vibe-os-native",
        "{init}"
    );
    // A notification gets nothing back.
    assert!(b.next(Duration::from_millis(500)).is_none());

    b.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
    let tools = b.expect();
    assert_eq!(tools["id"], 2);
    assert_eq!(tools["result"]["tools"].as_array().unwrap().len(), 8);

    b.send(
        json!({"jsonrpc": "2.0", "id": "three", "method": "tools/call", "params": {
        "name": "vibe_list_projects", "arguments": {}}}),
    );
    let r = b.expect();
    assert_eq!(r["id"], "three");
    let t = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(t.starts_with("app (on local)"), "{t}");

    // A wait's progress comes through as it happens, then its result.
    let a =
        w.rt.block_on(w.core.start_agent(AgentStartParams {
            project: "app".into(),
            profile: "software-eng-linux".into(),
            prompt: "hello".into(),
            ..Default::default()
        }))
        .unwrap();
    b.send(
        json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
        "name": "vibe_wait", "arguments": {"agents": a.agent, "timeout": 1, "lines": 0},
        "_meta": {"progressToken": "w"}}}),
    );
    let progress = b.expect();
    assert_eq!(progress["method"], "notifications/progress", "{progress}");
    assert_eq!(progress["params"]["progressToken"], "w");
    let mut r = b.expect();
    while r.get("method").is_some() {
        r = b.expect();
    }
    assert_eq!(r["id"], 4);
    let t = r["result"]["content"][0]["text"].as_str().unwrap();
    assert!(t.starts_with("still busy after 1s"), "{t}");

    // A cancelled call gets no response: the bridge closes its request,
    // which is how a stateless server hears of a cancel, and carries on.
    b.send(
        json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
        "name": "vibe_wait", "arguments": {"agents": a.agent, "timeout": 60, "lines": 0}}}),
    );
    std::thread::sleep(Duration::from_millis(500));
    b.send(
        json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 5}}),
    );
    let begun = Instant::now();
    b.send(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/list"}));
    let r = b.expect();
    assert_eq!(r["id"], 6, "{r}");
    assert!(
        b.next(Duration::from_secs(2)).is_none(),
        "nothing for the cancelled call"
    );
    assert!(begun.elapsed() < Duration::from_secs(10));

    // A 2026-07-28 client, with no initialize: its version and the headers
    // that go with it reach the server, and an error the server sends with
    // a 4xx comes back as the server wrote it.
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "new", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    b.send(
        json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {
        "name": "vibe_list_projects", "arguments": {}, "_meta": meta}}),
    );
    let r = b.expect();
    assert_eq!(r["id"], 7, "{r}");
    let t = r["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(t.starts_with("app (on local)"), "{r}");
    b.send(
        json!({"jsonrpc": "2.0", "id": 8, "method": "tools/list", "params": {
        "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}}),
    );
    let r = b.expect();
    assert_eq!(r["id"], 8, "{r}");
    assert_eq!(r["error"]["code"], -32602, "the server's own error: {r}");

    // A line that isn't JSON gets the protocol's parse error.
    b.stdin.as_mut().unwrap().write_all(b"not json\n").unwrap();
    let e = b.expect();
    assert_eq!(e["error"]["code"], -32700, "{e}");

    // stdin's end ends it.
    drop(b.stdin.take());
    let status = b.child.wait().unwrap();
    assert!(status.success());
    w.rt.block_on(w.core.delete_agent(vibe_core::rpc::AgentDeleteParams {
        host: None,
        agent: a.agent,
        force: Some(true),
    }))
    .unwrap();
}

#[test]
fn the_bridge_needs_a_key_the_daemon_knows() {
    let w = world();
    // None at all: it won't start.
    let out = Command::new(VIBE)
        .args(["mcp", "--url", &format!("http://{}/mcp", w.addr)])
        .env_remove("VIBE_API_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("vibe mcp needs an API key"), "{stderr}");
    // Not shaped like one.
    let out = Command::new(VIBE)
        .args([
            "mcp",
            "--key",
            "hunter2",
            "--url",
            &format!("http://{}/mcp", w.addr),
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("isn't an API key"));
    // One the daemon doesn't know: each request is answered with why.
    let unknown = vibe_core::keys::encode(&[9; 32]);
    let mut b = Bridge::start(&w, Some(&unknown));
    let r = b.initialize();
    assert_eq!(r["id"], 1);
    let m = r["error"]["message"].as_str().unwrap();
    assert!(m.contains("vibed refused the API key"), "{m}");
    // And nothing listening says where it tried.
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut cmd = Command::new(VIBE);
    let mut child = cmd
        .args(["mcp", "--url", &format!("http://{free}/mcp")])
        .env("VIBE_API_KEY", &w.key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})
    )
    .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let line = String::from_utf8_lossy(&out.stdout);
    assert!(
        line.contains(&format!("can't reach vibed's MCP at http://{free}/mcp")),
        "{line}"
    );
}

#[test]
fn keys_from_the_command_line() {
    let w = world();
    let vibe = |args: &[&str]| {
        Command::new(VIBE)
            .args(args)
            .env("VIBE_SOCKET", &w.socket)
            .env("HOME", &w.golden.home)
            .output()
            .unwrap()
    };
    let out = vibe(&["key", "create", "laptop"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let secret = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    assert!(vibe_core::keys::is_key(&secret), "{secret}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!(
            "claude mcp add --transport http vibeos http://127.0.0.1:7878/mcp --header \"Authorization: Bearer {secret}\""
        )),
        "{stderr}"
    );
    assert!(stderr.contains("equivalent to a shell"), "{stderr}");
    // It works over MCP, and says it was used.
    assert_eq!(
        vibe_mcp::Daemon::verify_key(&*w.core, &secret).as_deref(),
        Some("laptop")
    );
    let list = String::from_utf8(vibe(&["key", "list"]).stdout).unwrap();
    assert!(list.contains("bridge  vk_"), "{list}");
    assert!(list.contains("laptop  vk_"), "{list}");
    assert!(list.contains("last used 20"), "{list}");
    assert!(
        !list.contains(&secret[12..]),
        "only the start of a key shows: {list}"
    );
    let out = vibe(&["key", "revoke", "laptop"]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "revoked laptop\n");
    assert_eq!(vibe_mcp::Daemon::verify_key(&*w.core, &secret), None);
    let out = vibe(&["key", "revoke", "laptop"]);
    assert_eq!(out.status.code(), Some(1));
    // host.toml's port is the one the setup line names.
    std::fs::create_dir_all(w.golden.home.join(".config/vibeos")).unwrap();
    std::fs::write(
        w.golden.home.join(".config/vibeos/host.toml"),
        "mcp_port = 9123\n",
    )
    .unwrap();
    let out = vibe(&["key", "create", "other"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("http://127.0.0.1:9123/mcp"));
}
