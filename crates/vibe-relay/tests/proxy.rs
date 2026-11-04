//! The shim, driven as an MCP client drives it, with a stand-in for what's
//! behind its local socket. No network, and no Mac.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use vibe_relay::local::{self, Hello};
use vibe_relay::proxy::{self, Config, Timing};

fn timing() -> Timing {
    Timing {
        poll_connected: Duration::from_millis(100),
        poll_away: Duration::from_millis(50),
        first_list: Duration::from_millis(500),
    }
}

fn config(dir: &Path) -> Config {
    Config {
        service: "linear".into(),
        socket: dir.join("services.sock"),
        agent: Some("quiet-amber-otter".into()),
        cache: Some(dir.join("cache")),
        version: "1.2.3".into(),
        timing: timing(),
    }
}

/// An MCP client on a running shim.
struct Client {
    to: UnixStream,
    from: BufReader<UnixStream>,
    ended: std::sync::mpsc::Receiver<std::io::Result<()>>,
}

impl Client {
    fn start(config: Config) -> Client {
        let (client_in, shim_in) = UnixStream::pair().unwrap();
        let (client_out, shim_out) = UnixStream::pair().unwrap();
        client_out
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let (tx, ended) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(proxy::run(config, BufReader::new(shim_in), shim_out));
        });
        Client {
            to: client_in,
            from: BufReader::new(client_out),
            ended,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.to, "{message}").unwrap();
    }

    fn next(&mut self) -> Value {
        let mut line = String::new();
        self.from.read_line(&mut line).expect("the shim answers");
        assert!(!line.is_empty(), "the shim closed its stdout");
        serde_json::from_str(&line).unwrap()
    }

    /// Asks, and returns the answer, past anything else that comes first.
    fn ask(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let m = self.next();
            if m["id"] == json!(id) {
                return m;
            }
        }
    }

    fn handshake(&mut self) -> Value {
        let init = self.ask(
            1,
            "initialize",
            json!({"protocolVersion": "2026-07-28", "capabilities": {}}),
        );
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        init
    }

    fn call(&mut self, id: i64, tool: &str) -> Value {
        self.ask(id, "tools/call", json!({"name": tool, "arguments": {}}))
    }
}

fn tool_text(answer: &Value) -> String {
    answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

fn names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

/// What stands behind the local socket: a broker that knows some tools.
#[derive(Clone)]
struct Broker {
    tools: Arc<Mutex<Vec<&'static str>>>,
    hellos: Arc<Mutex<Vec<Hello>>>,
    calls: Arc<AtomicUsize>,
    conns: Arc<Mutex<Vec<UnixStream>>>,
}

impl Broker {
    fn serve(socket: &Path, tools: &[&'static str]) -> Broker {
        let broker = Broker {
            tools: Arc::new(Mutex::new(tools.to_vec())),
            hellos: Arc::default(),
            calls: Arc::default(),
            conns: Arc::default(),
        };
        let listener = UnixListener::bind(socket).unwrap();
        let b = broker.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let conn = conn.unwrap();
                let b = b.clone();
                std::thread::spawn(move || b.connection(conn));
            }
        });
        broker
    }

    fn connection(&self, conn: UnixStream) {
        self.conns.lock().unwrap().push(conn.try_clone().unwrap());
        let mut reader = BufReader::new(conn.try_clone().unwrap());
        let mut hello = String::new();
        reader.read_line(&mut hello).unwrap();
        self.hellos
            .lock()
            .unwrap()
            .push(Hello::parse(hello.trim_end().as_bytes()).unwrap());
        (&conn)
            .write_all(local::answer(&Ok(())).as_bytes())
            .unwrap();
        for line in reader.lines() {
            let Ok(line) = line else { return };
            let m: Value = serde_json::from_str(&line).unwrap();
            let answer = match m["method"].as_str() {
                Some("tools/list") => {
                    let tools: Vec<_> = self
                        .tools
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|t| json!({"name": t, "inputSchema": {"type": "object"}}))
                        .collect();
                    json!({"jsonrpc": "2.0", "id": m["id"], "result": {"tools": tools}})
                }
                Some("tools/call") if m["params"]["name"] == "hang_up" => {
                    let _ = conn.shutdown(std::net::Shutdown::Both);
                    return;
                }
                Some("tools/call") if m["params"]["name"] == "ask_back" => {
                    // A server's own request, then the call's answer once the
                    // client has answered it.
                    json!({"jsonrpc": "2.0", "id": 900, "method": "elicitation/create", "params": {"call": m["id"]}})
                }
                Some("tools/call") => {
                    self.calls.fetch_add(1, Ordering::SeqCst);
                    json!({"jsonrpc": "2.0", "id": m["id"], "result": {
                        "content": [{"type": "text", "text": format!("did {}", m["params"]["name"])}],
                    }})
                }
                None if m["id"] == json!(900) => {
                    json!({"jsonrpc": "2.0", "id": 77, "result": {
                        "content": [{"type": "text", "text": format!("told {}", m["result"]["ok"])}],
                    }})
                }
                _ => continue,
            };
            if writeln!(&conn, "{answer}").is_err() {
                return;
            }
        }
    }

    fn hang_up_everyone(&self) {
        for c in self.conns.lock().unwrap().drain(..) {
            let _ = c.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[test]
fn with_the_mac_away_the_handshake_works_and_calls_say_why_then_recover() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let socket: PathBuf = config.socket.clone();
    let mut c = Client::start(config);

    // A newer client's first question: answered, not a hang.
    let discover = c.ask(9, "server/discover", json!({}));
    assert_eq!(discover["error"]["code"], -32601);
    let init = c.handshake();
    assert_eq!(init["result"]["protocolVersion"], "2026-07-28");
    assert_eq!(init["result"]["serverInfo"]["name"], "vibe-linear");
    assert_eq!(init["result"]["capabilities"]["tools"]["listChanged"], true);
    assert_eq!(c.ask(2, "ping", json!({}))["result"], json!({}));
    assert_eq!(
        names(&c.ask(3, "tools/list", json!({}))),
        Vec::<String>::new()
    );

    // A tool error the model can read, not a protocol error, and not a hang.
    let down = c.call(4, "list_issues");
    assert_eq!(down["result"]["isError"], true, "{down}");
    let text = tool_text(&down);
    assert!(
        text.contains("VibeOS shared service 'linear' unavailable: Mac unreachable"),
        "{text}"
    );

    // The Mac comes back: the next call just works, on the same shim.
    let broker = Broker::serve(&socket, &["list_issues"]);
    let up = c.call(5, "list_issues");
    assert_eq!(tool_text(&up), "did \"list_issues\"", "{up}");
    assert_eq!(broker.calls.load(Ordering::SeqCst), 1);

    // And it said which agent it's for.
    assert!(broker.hellos.lock().unwrap().contains(&Hello::Open {
        service: "linear".into(),
        agent: Some("quiet-amber-otter".into()),
    }));
}

#[test]
fn a_new_catalog_is_announced_kept_and_listed_when_the_mac_is_away_again() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let broker = Broker::serve(&config.socket, &["list_issues"]);
    let mut c = Client::start(config.clone());
    c.handshake();
    assert_eq!(names(&c.ask(2, "tools/list", json!({}))), ["list_issues"]);

    broker.tools.lock().unwrap().push("create_issue");
    // The first catalog was announced too, so one notice may be that one's:
    // what counts is that a notice comes after which the new tool lists.
    let mut id = 3;
    loop {
        if c.next()["method"] == "notifications/tools/list_changed" {
            id += 1;
            let listed = names(&c.ask(id, "tools/list", json!({})));
            if listed == ["list_issues", "create_issue"] {
                break;
            }
        }
    }

    // A new session, with the socket gone: tools still list, from disk.
    drop(c);
    let elsewhere = tempfile::tempdir().unwrap();
    let offline = Config {
        socket: elsewhere.path().join("nothing.sock"),
        ..config
    };
    let mut c = Client::start(offline);
    c.handshake();
    assert_eq!(
        names(&c.ask(2, "tools/list", json!({}))),
        ["list_issues", "create_issue"]
    );
}

#[test]
fn a_connection_lost_during_a_call_answers_that_call_and_the_next_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let broker = Broker::serve(&config.socket, &["list_issues", "hang_up"]);
    let mut c = Client::start(config);
    c.handshake();
    let lost = c.call(2, "hang_up");
    assert_eq!(lost["result"]["isError"], true, "{lost}");
    assert!(tool_text(&lost).contains("Mac unreachable"), "{lost}");
    assert_eq!(tool_text(&c.call(3, "list_issues")), "did \"list_issues\"");

    broker.hang_up_everyone();
    assert_eq!(tool_text(&c.call(4, "list_issues")), "did \"list_issues\"");
}

#[test]
fn a_call_on_a_new_link_isnt_failed_by_the_old_links_end() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    let broker = Broker::serve(&config.socket, &["list_issues"]);
    let mut c = Client::start(config);
    c.handshake();
    // Each round leaves an old link's reader finishing just as the next call
    // goes out on a new one.
    for id in 2..60 {
        broker.hang_up_everyone();
        let answer = c.call(id, "list_issues");
        assert_eq!(tool_text(&answer), "did \"list_issues\"", "{answer}");
    }
}

#[test]
fn a_servers_own_request_reaches_the_client_and_its_answer_goes_back() {
    let dir = tempfile::tempdir().unwrap();
    let config = config(dir.path());
    Broker::serve(&config.socket, &["ask_back"]);
    let mut c = Client::start(config);
    c.handshake();
    c.send(
        json!({"jsonrpc": "2.0", "id": 77, "method": "tools/call", "params": {"name": "ask_back"}}),
    );
    let asked = loop {
        let m = c.next();
        if m["method"] == "elicitation/create" {
            break m;
        }
    };
    c.send(json!({"jsonrpc": "2.0", "id": asked["id"], "result": {"ok": 1}}));
    let done = loop {
        let m = c.next();
        if m["id"] == json!(77) {
            break m;
        }
    };
    assert_eq!(tool_text(&done), "told 1");
}

#[test]
fn it_ends_only_when_its_client_does() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = Client::start(config(dir.path()));
    c.handshake();
    // Rubbish from a client is answered, and ends nothing.
    writeln!(c.to, "not json").unwrap();
    assert_eq!(c.next()["error"]["code"], -32700);
    writeln!(c.to, "[]").unwrap();
    assert_eq!(c.next()["error"]["code"], -32600);
    assert_eq!(c.ask(2, "ping", json!({}))["result"], json!({}));
    assert!(c.ended.try_recv().is_err());
    drop(c.to);
    assert!(
        c.ended
            .recv_timeout(Duration::from_secs(20))
            .unwrap()
            .is_ok()
    );
}
