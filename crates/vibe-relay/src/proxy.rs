//! The shim: `vibe mcp-proxy <service>`, an MCP server on stdio for an agent's
//! client, which forwards tool calls to its host's broker.
//!
//! It never exits while its client lives, and answers the handshake itself:
//! `initialize` and `tools/list` come from here, from a catalog kept on disk,
//! so tools load even when the broker can't be reached as the session starts
//! (a shim that forwarded them would leave the server failed for the whole
//! session). Each call connects if it has to, and while the broker is away gets
//! a clear tool error, never a hang; the next call after it's back just works.
//! When the catalog the broker gives differs from the one on disk, the client
//! is told with `notifications/tools/list_changed`.
//!
//! stdout is the protocol channel, so nothing else is ever written there.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::local::{self, Hello};
use crate::{read_line, server_name};

/// What's offered when a client names no protocol version. Otherwise its own
/// is agreed: all this serves is `tools/*`, the same in every revision.
const DEFAULT_PROTOCOL: &str = "2025-06-18";

const CATALOG_ID: &str = "vibe-catalog-";

/// JSON-RPC's code for a method nobody serves.
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_REQUEST: i64 = -32600;

/// How long a hello's answer may take: the local socket answers at once.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a write to the local socket may block.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How the shim is timed; tests make it quicker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// How often the catalog is asked for while connected.
    pub poll_connected: Duration,
    /// How often the broker is tried while it's away.
    pub poll_away: Duration,
    /// With nothing on disk, how long a client's first `tools/list` waits for
    /// the live catalog before it's told there are no tools yet.
    pub first_list: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            poll_connected: Duration::from_secs(5 * 60),
            poll_away: Duration::from_secs(30),
            first_list: Duration::from_secs(3),
        }
    }
}

/// What the shim is to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub service: String,
    /// The local socket: `vibe-guest`'s, or `vibed`'s.
    pub socket: PathBuf,
    /// The agent it's in, if it's in one.
    pub agent: Option<String>,
    /// Where catalogs are kept, if anywhere: a directory.
    pub cache: Option<PathBuf>,
    pub version: String,
    pub timing: Timing,
}

/// A `tools/call`'s answer when it failed in a way the agent should read:
/// an MCP tool error, not a protocol error, which a model never sees.
pub fn tool_error(id: &Value, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {"content": [{"type": "text", "text": text}], "isError": true},
    })
}

/// What an agent reads when its broker can't be reached.
pub fn unavailable(service: &str, why: &str) -> String {
    format!(
        "VibeOS shared service '{service}' unavailable: {why}. It works again by itself once \
         that's back; nothing needs reconnecting."
    )
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Catalog {
    tools: Vec<Value>,
    instructions: Option<String>,
}

impl Catalog {
    fn from_result(result: &Value) -> Option<Catalog> {
        Some(Catalog {
            tools: result.get("tools")?.as_array()?.clone(),
            instructions: result
                .get("instructions")
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn to_result(&self) -> Value {
        let mut v = json!({"tools": self.tools});
        if let Some(i) = &self.instructions {
            v["instructions"] = json!(i);
        }
        v
    }
}

struct Link {
    writer: UnixStream,
    generation: u64,
}

#[derive(Default)]
struct State {
    catalog: Catalog,
    /// The catalog has come from the broker, not just from disk.
    live: bool,
    /// The client said `notifications/initialized`.
    ready: bool,
    /// Calls forwarded and not yet answered, each with the link it went out
    /// on: if that link drops they get an answer, but a call already on a
    /// newer link isn't touched.
    in_flight: Vec<(Value, u64)>,
}

struct Shim {
    config: Config,
    out: Mutex<Box<dyn Write + Send>>,
    link: Mutex<Option<Link>>,
    state: Mutex<State>,
    catalog_came: Condvar,
    next_generation: AtomicU64,
    next_catalog: AtomicU64,
    closing: AtomicBool,
}

impl Shim {
    fn write(&self, message: &Value) -> io::Result<()> {
        let mut out = lock(&self.out);
        writeln!(out, "{message}")?;
        out.flush()
    }

    fn cache_file(&self) -> Option<PathBuf> {
        Some(
            self.config
                .cache
                .as_ref()?
                .join(format!("{}.json", self.config.service)),
        )
    }

    fn load_catalog(&self) -> Option<Catalog> {
        let text = fs::read(self.cache_file()?).ok()?;
        Catalog::from_result(&serde_json::from_slice(&text).ok()?)
    }

    /// Keeps the catalog on disk, whole or not at all.
    fn save_catalog(&self, catalog: &Catalog) {
        let Some(file) = self.cache_file() else {
            return;
        };
        let tmp = file.with_extension(format!("{}.new", std::process::id()));
        let written = file
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::write(&tmp, catalog.to_result().to_string()))
            .and_then(|()| fs::rename(&tmp, &file));
        if let Err(e) = written {
            eprintln!("vibe mcp-proxy: can't keep the catalog: {e}");
            let _ = fs::remove_file(&tmp);
        }
    }

    /// Connects, if it isn't. `Err` says why not, for the agent to read.
    fn connect(self: &Arc<Self>) -> Result<(), String> {
        let mut link = lock(&self.link);
        if link.is_some() {
            return Ok(());
        }
        let conn = UnixStream::connect(&self.config.socket).map_err(|e| {
            format!(
                "VibeOS isn't answering on this machine: nothing at {} ({e})",
                self.config.socket.display()
            )
        })?;
        let broke = |e: io::Error| format!("VibeOS's connection broke ({e})");
        conn.set_write_timeout(Some(WRITE_TIMEOUT)).map_err(broke)?;
        conn.set_read_timeout(Some(CONNECT_TIMEOUT))
            .map_err(broke)?;
        let hello = Hello {
            service: self.config.service.clone(),
            agent: self.config.agent.clone(),
        };
        (&conn).write_all(hello.line().as_bytes()).map_err(broke)?;
        let mut reader = BufReader::new(conn.try_clone().map_err(broke)?);
        let answer = read_line(&mut reader)
            .map_err(broke)?
            .ok_or_else(|| "VibeOS closed the connection".to_owned())?;
        local::parse_answer(&answer)?;
        conn.set_read_timeout(None).map_err(broke)?;
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        *link = Some(Link {
            writer: conn,
            generation,
        });
        drop(link);
        let shim = self.clone();
        std::thread::Builder::new()
            .name("mcp-proxy-link".into())
            .spawn(move || shim.read_link(reader, generation))
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// Sends a line to the broker, connecting first if need be, and once more
    /// if the link had gone without saying.
    fn send(self: &Arc<Self>, line: &str) -> Result<(), String> {
        self.send_call(line, None)
    }

    /// The same, for a call whose id is to be answered if its link drops. It's
    /// recorded before it's written, with the link held, so neither its answer
    /// nor that link's end can come first.
    fn send_call(self: &Arc<Self>, line: &str, call: Option<&Value>) -> Result<(), String> {
        let mut last = String::new();
        for _ in 0..2 {
            self.connect()?;
            let mut link = lock(&self.link);
            let Some(l) = link.as_mut() else {
                continue;
            };
            if let Some(id) = call {
                lock(&self.state).in_flight.push((id.clone(), l.generation));
            }
            match (&l.writer)
                .write_all(line.as_bytes())
                .and_then(|()| (&l.writer).write_all(b"\n"))
            {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if let Some(id) = call {
                        self.forget_call(id);
                    }
                    last = format!("VibeOS's connection broke ({e})");
                    let _ = l.writer.shutdown(std::net::Shutdown::Both);
                    *link = None;
                }
            }
        }
        Err(last)
    }

    fn forget_call(&self, id: &Value) {
        lock(&self.state).in_flight.retain(|(i, _)| i != id);
    }

    fn request_catalog(self: &Arc<Self>) -> Result<(), String> {
        let n = self.next_catalog.fetch_add(1, Ordering::Relaxed);
        self.send(
            &json!({"jsonrpc": "2.0", "id": format!("{CATALOG_ID}{n}"), "method": "tools/list"})
                .to_string(),
        )
    }

    fn read_link(self: Arc<Self>, mut reader: BufReader<UnixStream>, generation: u64) {
        while let Ok(Some(line)) = read_line(&mut reader) {
            if let Ok(message) = serde_json::from_slice::<Value>(&line) {
                self.from_broker(message);
            }
        }
        let mut link = lock(&self.link);
        if link.as_ref().is_some_and(|l| l.generation == generation) {
            *link = None;
        }
        drop(link);
        // Every call under way on this link gets its answer: it won't get
        // another.
        let ids: Vec<Value> = {
            let mut state = lock(&self.state);
            let (lost, kept) = std::mem::take(&mut state.in_flight)
                .into_iter()
                .partition(|(_, on)| *on == generation);
            state.in_flight = kept;
            lost.into_iter().map(|(id, _): (Value, u64)| id).collect()
        };
        let text = unavailable(
            &self.config.service,
            "VibeOS's connection dropped during this call",
        );
        for id in ids {
            let _ = self.write(&tool_error(&id, &text));
        }
    }

    fn from_broker(self: &Arc<Self>, message: Value) {
        let method = message.get("method").and_then(Value::as_str);
        let id = message.get("id");
        if let (None, Some(id)) = (method, id) {
            if id.as_str().is_some_and(|i| i.starts_with(CATALOG_ID)) {
                if let Some(catalog) = message.get("result").and_then(Catalog::from_result) {
                    self.new_catalog(catalog);
                }
                return;
            }
            self.forget_call(id);
        }
        if method == Some("notifications/tools/list_changed") {
            // Ours goes out once the new catalog's here.
            let _ = self.request_catalog();
            return;
        }
        if self.write(&message).is_err() {
            self.closing.store(true, Ordering::Relaxed);
        }
    }

    fn new_catalog(&self, catalog: Catalog) {
        let mut state = lock(&self.state);
        state.live = true;
        let changed = state.catalog != catalog;
        if changed {
            state.catalog = catalog.clone();
        }
        let ready = state.ready;
        self.catalog_came.notify_all();
        drop(state);
        if changed {
            self.save_catalog(&catalog);
            if ready {
                let _ = self.write(&json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/tools/list_changed",
                }));
            }
        }
    }

    /// Asks for the catalog on a timer, which is also how a broker that's come
    /// back is noticed between calls.
    fn poll(self: Arc<Self>) {
        let mut due = Instant::now();
        while !self.closing.load(Ordering::Relaxed) {
            if Instant::now() >= due {
                let every = match self.request_catalog() {
                    Ok(()) => self.config.timing.poll_connected,
                    Err(_) => self.config.timing.poll_away,
                };
                due = Instant::now() + every;
            }
            // Rarely awake, as an idle agent's processes should be: it's asleep
            // until it's due, and looks in now and then to see if it's over.
            let until_due = due.saturating_duration_since(Instant::now());
            std::thread::sleep(until_due.clamp(Duration::from_millis(1), Duration::from_secs(1)));
        }
    }

    fn from_client(self: &Arc<Self>, line: &[u8]) -> io::Result<()> {
        let Ok(message) = serde_json::from_slice::<Value>(line) else {
            return self.write(&error(&Value::Null, -32700, "Parse error"));
        };
        if !message.is_object() {
            return self.write(&error(&Value::Null, INVALID_REQUEST, "Invalid Request"));
        }
        let id = message.get("id");
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A client's answer to something the server asked it.
            let _ = self.send(&message.to_string());
            return Ok(());
        };
        let Some(id) = id else {
            return self.notification(method, &message);
        };
        match method {
            "initialize" => {
                let requested = message
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or(DEFAULT_PROTOCOL);
                let mut result = json!({
                    "protocolVersion": requested,
                    "capabilities": {"tools": {"listChanged": true}},
                    "serverInfo": {
                        "name": server_name(&self.config.service),
                        "version": self.config.version,
                    },
                });
                if let Some(i) = &lock(&self.state).catalog.instructions {
                    result["instructions"] = json!(i);
                }
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
            "ping" => self.write(&json!({"jsonrpc": "2.0", "id": id, "result": {}})),
            "tools/list" => {
                let result = self.catalog_for_list().to_result();
                self.write(&json!({"jsonrpc": "2.0", "id": id, "result": result}))
            }
            "tools/call" => self.call(id, &message),
            // Newer clients ask `server/discover` before `initialize`; only
            // tools are offered, so anything else is answered the same way.
            other => self.write(&error(
                id,
                METHOD_NOT_FOUND,
                &format!("Method not found: {other}"),
            )),
        }
    }

    fn notification(self: &Arc<Self>, method: &str, message: &Value) -> io::Result<()> {
        match method {
            "notifications/initialized" => lock(&self.state).ready = true,
            "notifications/cancelled" => {
                if let Some(req) = message.pointer("/params/requestId") {
                    self.forget_call(req);
                }
                let _ = self.send(&message.to_string());
            }
            _ => {
                let _ = self.send(&message.to_string());
            }
        }
        Ok(())
    }

    /// The catalog to list. With nothing kept, the broker gets a moment to give
    /// its own, so a session starting now usually has its tools at once;
    /// `list_changed` covers the rest.
    fn catalog_for_list(self: &Arc<Self>) -> Catalog {
        let known = {
            let state = lock(&self.state);
            state.live || !state.catalog.tools.is_empty()
        };
        if !known && self.request_catalog().is_ok() {
            let deadline = Instant::now() + self.config.timing.first_list;
            let mut state = lock(&self.state);
            while !state.live {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = self
                    .catalog_came
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        lock(&self.state).catalog.clone()
    }

    fn call(self: &Arc<Self>, id: &Value, message: &Value) -> io::Result<()> {
        if let Err(why) = self.send_call(&message.to_string(), Some(id)) {
            return self.write(&tool_error(id, &unavailable(&self.config.service, &why)));
        }
        Ok(())
    }
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Runs the shim until its client closes `input`.
pub fn run(
    config: Config,
    mut input: impl BufRead,
    output: impl Write + Send + 'static,
) -> io::Result<()> {
    let shim = Arc::new(Shim {
        config,
        out: Mutex::new(Box::new(output)),
        link: Mutex::new(None),
        state: Mutex::default(),
        catalog_came: Condvar::new(),
        next_generation: AtomicU64::new(1),
        next_catalog: AtomicU64::new(1),
        closing: AtomicBool::new(false),
    });
    if let Some(catalog) = shim.load_catalog() {
        lock(&shim.state).catalog = catalog;
    }
    {
        let shim = shim.clone();
        std::thread::Builder::new()
            .name("mcp-proxy-poll".into())
            .spawn(move || shim.poll())?;
    }
    let mut ended = Ok(());
    loop {
        match read_line(&mut input) {
            Ok(Some(line)) if line.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Some(line)) => {
                // Its client has gone if it can't be written to.
                if let Err(e) = shim.from_client(&line) {
                    ended = Err(e);
                    break;
                }
            }
            Ok(None) => break,
            // A line too long for the protocol: the client's error, not an
            // end. Whatever followed in it is lost with it.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                eprintln!("vibe mcp-proxy: {e}");
            }
            Err(e) => {
                ended = Err(e);
                break;
            }
        }
    }
    shim.closing.store(true, Ordering::Relaxed);
    if let Some(l) = lock(&shim.link).take() {
        let _ = l.writer.shutdown(std::net::Shutdown::Both);
    }
    ended
}
