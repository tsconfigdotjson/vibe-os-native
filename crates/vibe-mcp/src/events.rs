//! MCP Events (SEP-3415, `io.modelcontextprotocol/events`; PRD: MCP
//! interface, Events): a client subscribes to what happens to agents
//! instead of calling `vibe_wait` and `vibe_list_agents` over and over.
//!
//! Four event types, about agents on every host this endpoint reaches:
//! this one, the Linux hosts linked to it, and on a Linux host the Mac
//! behind it and the other hosts linked to that Mac. `agent.started`,
//! `agent.state_changed`, `agent.turn_ended` and `agent.deleted`. Each
//! payload is a ref, its host, project and state, and no more: what an
//! agent said or shows is read with the tools, so nothing an agent wrote
//! rides in an event.
//!
//! Delivery is by webhook ([`crate::webhook`]), which works between a
//! client's turns, as ChatGPT has it.
//!
//! Subscriptions read one [`Hub`] per daemon: a ring of the last [`RING`]
//! occurrences, fed from the daemon's event bus. A cursor is a position in
//! it, `<boot>-<seq>`; one from an earlier `vibed`, or older than the ring
//! holds, is `truncated`, as are the times a host's events may have been
//! missed (its link was down, or the feed fell behind).
//!
//! What another daemon relays (on a Linux host, the Mac's events, down its
//! tunnel) is held apart as [`Occurrence::relayed`]: that daemon checks keys
//! itself, so its events, and the gaps in them, go only to subscriptions
//! whose owner it takes ([`crate::webhook::Owners::hears_relayed`]).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, watch};
use vibe_core::model::{AgentState, EventKind, LOCAL_HOST};
use vibe_core::rpc::Event;

use crate::{display_ref, split_ref};

/// The extension's identifier, the key it's advertised under.
pub const EXTENSION: &str = "io.modelcontextprotocol/events";

/// How many occurrences the hub keeps for replay.
pub const RING: usize = 4096;

const MAX_FILTER_AGENTS: usize = 32;

// The extension's error codes (Error Handling).
pub(crate) const NOT_FOUND: i32 = -32023;
pub(crate) const RESOURCE_EXHAUSTED: i32 = -32025;
pub(crate) const UNSUPPORTED: i32 = -32026;

pub(crate) fn error(code: i32, message: impl Into<String>, data: Option<Value>) -> ErrorData {
    ErrorData::new(ErrorCode(code), message.into(), data)
}

fn invalid(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

/// An event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Name {
    Started,
    StateChanged,
    TurnEnded,
    Deleted,
}

impl Name {
    pub const ALL: [Name; 4] = [
        Name::Started,
        Name::StateChanged,
        Name::TurnEnded,
        Name::Deleted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Name::Started => "agent.started",
            Name::StateChanged => "agent.state_changed",
            Name::TurnEnded => "agent.turn_ended",
            Name::Deleted => "agent.deleted",
        }
    }

    pub fn parse(s: &str) -> Option<Name> {
        Name::ALL.into_iter().find(|n| n.as_str() == s)
    }

    fn description(self) -> &'static str {
        match self {
            Name::Started => {
                "An agent was started, by any client or the app, on any host this server reaches."
            }
            Name::StateChanged => {
                "An agent changed state: busy, idle, question (it's asking something), asleep, paused (a limit hit) or exited. previous is its state before, when known."
            }
            Name::TurnEnded => {
                "An agent stopped working: it was busy and now isn't. state says where it stopped: idle (it has answered; read its answer with vibe_read_messages), question (it's asking something; read it with vibe_read_output and answer with vibe_send), or asleep, paused or exited."
            }
            Name::Deleted => "An agent was deleted, with its worktree. Its branch is kept.",
        }
    }

    /// Whether `state` filters it.
    fn has_state(self) -> bool {
        matches!(self, Name::StateChanged | Name::TurnEnded)
    }

    /// The states it can carry, for its schemas.
    fn states(self) -> &'static [&'static str] {
        match self {
            Name::StateChanged => &STATES,
            Name::TurnEnded => &STATES[1..],
            _ => &[],
        }
    }

    fn input_schema(self) -> Value {
        let mut properties = json!({
            "host": {"type": "string", "description": "Only agents on this host, like local or vps"},
            "project": {"type": "string", "description": "Only agents of this project, by name"},
            "agents": {
                "description": "Only these agents: one ref, or several. A bare ref, like quiet-amber-otter, matches it on any host; vps:quiet-amber-otter only on vps",
                "anyOf": [
                    {"type": "string"},
                    {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_FILTER_AGENTS}
                ]
            }
        });
        if self.has_state() {
            let states = self.states();
            properties["state"] = json!({
                "description": "Only when it changes to this state, or one of these",
                "anyOf": [
                    {"type": "string", "enum": states},
                    {"type": "array", "items": {"type": "string", "enum": states}, "minItems": 1}
                ]
            });
        }
        json!({"type": "object", "properties": properties, "additionalProperties": false})
    }

    fn payload_schema(self) -> Value {
        let mut properties = json!({
            "ref": {"type": "string", "description": "The agent's ref as the tools take it: bare on this host, host:ref on another"},
            "host": {"type": "string"},
            "agent": {"type": "string", "description": "Its ref on its own host"},
            "project": {"type": ["string", "null"]},
        });
        let mut required = vec!["ref", "host", "agent", "project"];
        match self {
            Name::Started | Name::StateChanged | Name::TurnEnded => {
                let states: &[&str] = if self == Name::Started {
                    &STATES
                } else {
                    self.states()
                };
                properties["state"] = json!({"type": "string", "enum": states});
                required.push("state");
            }
            Name::Deleted => {}
        }
        if self == Name::StateChanged {
            properties["previous"] = json!({"type": ["string", "null"], "enum": STATES.iter().map(|s| Value::from(*s)).chain([Value::Null]).collect::<Vec<_>>(), "description": "Its state before, or null when it isn't known"});
            required.push("previous");
        }
        json!({"type": "object", "properties": properties, "required": required})
    }

    /// Its `events/list` entry.
    fn descriptor(self) -> Value {
        json!({
            "name": self.as_str(),
            "description": self.description(),
            "delivery": ["webhook"],
            "inputSchema": self.input_schema(),
            "payloadSchema": self.payload_schema(),
        })
    }
}

/// Every agent state, busy first.
const STATES: [&str; 6] = ["busy", "idle", "question", "asleep", "paused", "exited"];

/// One thing that happened to an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Occurrence {
    pub seq: u64,
    pub name: Name,
    /// When it happened, as its host said.
    pub timestamp: String,
    pub host: String,
    pub agent: String,
    pub project: Option<String>,
    pub state: Option<String>,
    pub previous: Option<String>,
    /// Relayed by another daemon, which decides who hears it: only callers
    /// it takes do.
    pub relayed: bool,
}

impl Occurrence {
    /// Its payload: what a delivery's `data` is.
    pub fn data(&self) -> Value {
        let mut d = json!({
            "ref": display_ref(&self.host, &self.agent),
            "host": self.host,
            "agent": self.agent,
            "project": self.project,
        });
        if self.name != Name::Deleted {
            d["state"] = json!(self.state);
        }
        if self.name == Name::StateChanged {
            d["previous"] = json!(self.previous);
        }
        d
    }
}

/// What the ring holds: an occurrence, or a point where events may have
/// been missed, for one host or (`None`) every host; of those relayed
/// alone, if `relayed`.
#[derive(Debug, Clone)]
enum Entry {
    Event(Occurrence),
    Gap {
        seq: u64,
        host: Option<String>,
        relayed: bool,
    },
}

impl Entry {
    fn seq(&self) -> u64 {
        match self {
            Entry::Event(o) => o.seq,
            Entry::Gap { seq, .. } => *seq,
        }
    }
}

/// A read: what came after a position, and the position after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub events: Vec<Occurrence>,
    pub cursor: u64,
    pub has_more: bool,
}

#[derive(Debug, Default)]
struct Inner {
    ring: VecDeque<Entry>,
    /// The last `seq` given out; 0 before the first.
    last: u64,
    /// Each agent's last state the hub heard, by host and ref.
    states: HashMap<(String, String), String>,
    /// Each linked host's link, as last heard.
    links: HashMap<String, String>,
}

impl Inner {
    fn push(&mut self, entry: impl FnOnce(u64) -> Entry) {
        self.last += 1;
        if self.ring.len() == RING {
            self.ring.pop_front();
        }
        self.ring.push_back(entry(self.last));
    }

    /// The first position the ring can serve from: everything after it is
    /// still held.
    fn floor(&self) -> u64 {
        self.ring.front().map_or(self.last, |e| e.seq() - 1)
    }
}

/// The daemon's events, shared by every listener and request: `rmcp` makes
/// a server per request, so nothing of a subscription lives anywhere else.
#[derive(Debug)]
pub struct Hub {
    /// Names this process's cursors, so one from an earlier `vibed` is told
    /// apart.
    boot: String,
    inner: Mutex<Inner>,
    /// The last `seq`, for deliveries to wait on.
    newest: watch::Sender<u64>,
    /// Bumped when the API keys change, so subscriptions check theirs again.
    keys: watch::Sender<u64>,
}

impl Default for Hub {
    fn default() -> Self {
        Hub::new()
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// A cursor: `<boot>-<seq>`.
fn position(boot: &str, seq: u64) -> String {
    format!("{boot}-{seq}")
}

impl Hub {
    pub fn new() -> Hub {
        let boot = match vibe_core::keys::random_bytes() {
            Ok(b) => b[..6].iter().map(|b| format!("{b:02x}")).collect(),
            // Only told apart from another process's by the time, then.
            Err(_) => format!("{:x}", now_ms()),
        };
        Hub {
            boot,
            inner: Mutex::new(Inner::default()),
            newest: watch::Sender::new(0),
            keys: watch::Sender::new(0),
        }
    }

    /// What the hub knows of an agent's state before it hears of a change,
    /// as the daemon has it at the start: so the first change it hears of
    /// has a `previous`, and a turn that ends is seen to.
    pub fn seed(&self, host: &str, agent: &str, state: AgentState) {
        lock(&self.inner)
            .states
            .entry((host.to_owned(), agent.to_owned()))
            .or_insert_with(|| state.to_string());
    }

    /// Takes in an event from the daemon's bus.
    pub fn ingest(&self, e: &Event) {
        self.take_in(e, false);
    }

    /// Takes in an event another daemon relayed, its host as it goes by
    /// here: only callers that daemon takes hear of it.
    pub fn ingest_relayed(&self, e: &Event) {
        if e.host != LOCAL_HOST {
            self.take_in(e, true);
        }
    }

    /// What a relay may have missed, on every host it relays: it's only now
    /// (again) heard from.
    pub fn relayed_gap(&self) {
        let mut inner = lock(&self.inner);
        inner.push(|seq| Entry::Gap {
            seq,
            host: None,
            relayed: true,
        });
        let last = inner.last;
        drop(inner);
        self.newest.send_replace(last);
    }

    /// An agent's state as a relay says it is now, over what was heard
    /// before: what changed in between was missed.
    pub fn seed_relayed(&self, host: &str, agent: &str, state: &str) {
        lock(&self.inner)
            .states
            .insert((host.to_owned(), agent.to_owned()), state.to_owned());
    }

    /// How a linked host's link stands as the hub starts hearing of it, so
    /// only its coming back up afterwards is a gap.
    pub fn seed_link(&self, host: &str, state: &str) {
        lock(&self.inner)
            .links
            .insert(host.to_owned(), state.to_owned());
    }

    fn take_in(&self, e: &Event, relayed: bool) {
        let mut inner = lock(&self.inner);
        let before = inner.last;
        let occurrence = |seq, name, state: Option<String>, previous| {
            Entry::Event(Occurrence {
                seq,
                name,
                timestamp: e.at.clone(),
                host: e.host.clone(),
                agent: e.agent.clone().unwrap_or_default(),
                project: e.project.clone(),
                state,
                previous,
                relayed,
            })
        };
        match e.kind {
            EventKind::AgentAdded | EventKind::AgentState | EventKind::AgentDeleted
                if e.agent.is_none() => {}
            EventKind::AgentAdded => {
                let state = e.state.clone().unwrap_or_else(|| "busy".into());
                inner.states.insert(
                    (e.host.clone(), e.agent.clone().unwrap_or_default()),
                    state.clone(),
                );
                inner.push(|seq| occurrence(seq, Name::Started, Some(state), None));
            }
            EventKind::AgentState => {
                let Some(state) = e.state.clone() else { return };
                let key = (e.host.clone(), e.agent.clone().unwrap_or_default());
                let previous = inner.states.insert(key, state.clone());
                if previous.as_ref() == Some(&state) {
                    return;
                }
                inner.push(|seq| {
                    occurrence(
                        seq,
                        Name::StateChanged,
                        Some(state.clone()),
                        previous.clone(),
                    )
                });
                if previous.as_deref() == Some("busy") {
                    inner.push(|seq| occurrence(seq, Name::TurnEnded, Some(state), None));
                }
            }
            EventKind::AgentDeleted => {
                inner
                    .states
                    .remove(&(e.host.clone(), e.agent.clone().unwrap_or_default()));
                inner.push(|seq| occurrence(seq, Name::Deleted, None, None));
            }
            // A linked host's link coming up: whatever happened there while
            // it was down, or before it first came up, was never heard.
            EventKind::HostLink if e.host != LOCAL_HOST => {
                let state = e.state.clone().unwrap_or_default();
                let was = inner.links.insert(e.host.clone(), state.clone());
                if state == "connected" && was.as_deref() != Some("connected") {
                    let host = e.host.clone();
                    inner.push(|seq| Entry::Gap {
                        seq,
                        host: Some(host),
                        relayed,
                    });
                }
            }
            // A linked host's own feed fell behind.
            EventKind::Lagged if e.host != LOCAL_HOST => {
                let host = e.host.clone();
                inner.push(|seq| Entry::Gap {
                    seq,
                    host: Some(host),
                    relayed,
                });
            }
            EventKind::KeysChanged if !relayed => {
                self.keys.send_modify(|n| *n += 1);
            }
            _ => {}
        }
        let last = inner.last;
        drop(inner);
        if last != before {
            self.newest.send_replace(last);
        }
    }

    /// The hub's own feed fell behind the bus: anything may be missing.
    pub fn lagged(&self) {
        let mut inner = lock(&self.inner);
        inner.push(|seq| Entry::Gap {
            seq,
            host: None,
            relayed: false,
        });
        let last = inner.last;
        drop(inner);
        self.newest.send_replace(last);
    }

    /// Feeds the hub from the daemon's bus until the bus closes.
    pub fn spawn_feed(self: &Arc<Self>, mut rx: broadcast::Receiver<Event>) {
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(e) => hub.ingest(&e),
                    Err(broadcast::error::RecvError::Lagged(_)) => hub.lagged(),
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        });
    }

    /// The last position.
    pub fn last(&self) -> u64 {
        lock(&self.inner).last
    }

    /// A position, as a cursor.
    pub fn cursor(&self, seq: u64) -> String {
        position(&self.boot, seq)
    }

    /// The last position, as it changes.
    pub fn watch_newest(&self) -> watch::Receiver<u64> {
        self.newest.subscribe()
    }

    /// Counts the times the API keys changed, as they do.
    pub fn watch_keys(&self) -> watch::Receiver<u64> {
        self.keys.subscribe()
    }

    /// Whether `filter` may have missed something after position `after`:
    /// a gap that may hold one of its, or the ring having moved on past it.
    pub fn gap_after(&self, after: u64, filter: &Filter) -> bool {
        let inner = lock(&self.inner);
        after < inner.floor()
            || inner.ring.iter().any(|e| match e {
                Entry::Gap { seq, host, relayed } => {
                    *seq > after
                        && (!relayed || filter.relayed)
                        && host.as_deref().is_none_or(|h| filter.may_include_host(h))
                }
                Entry::Event(_) => false,
            })
    }

    /// Where a client's cursor says to go on from, and whether that's later
    /// than it asked (`truncated`): `None` is now, as is a cursor from an
    /// earlier `vibed`.
    pub fn resolve(&self, cursor: Option<&str>) -> Result<(u64, bool), ErrorData> {
        let last = lock(&self.inner).last;
        let Some(cursor) = cursor else {
            return Ok((last, false));
        };
        let bad = || invalid(format!("cursor {cursor:?} isn't one this server gave"));
        let (boot, seq) = cursor.rsplit_once('-').ok_or_else(bad)?;
        let seq: u64 = seq.parse().map_err(|_| bad())?;
        if boot != self.boot {
            // Its events went with the process that gave it.
            return Ok((last, true));
        }
        if seq > last {
            return Err(bad());
        }
        Ok((seq, false))
    }

    /// What `filter` lets through after position `after`, up to `max`
    /// events: from what the ring still holds, if it's moved on past
    /// `after`. What may have been missed is [`Hub::gap_after`]'s to say.
    pub fn read(&self, after: u64, filter: &Filter, max: usize) -> Read {
        let inner = lock(&self.inner);
        let start = after.max(inner.floor());
        let mut events = Vec::new();
        let mut cursor = start;
        let mut has_more = false;
        for entry in inner.ring.iter().filter(|e| e.seq() > start) {
            if let Entry::Event(o) = entry
                && filter.matches(o)
            {
                if events.len() == max {
                    has_more = true;
                    break;
                }
                events.push(o.clone());
            }
            cursor = entry.seq();
        }
        if !has_more {
            cursor = inner.last.max(cursor);
        }
        Read {
            events,
            cursor,
            has_more,
        }
    }
}

/// A subscription's arguments, checked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    pub name: Option<Name>,
    pub host: Option<String>,
    pub project: Option<String>,
    /// Refs, each with its host if it names one.
    pub agents: Option<Vec<(Option<String>, String)>>,
    pub states: Option<Vec<String>>,
    /// Whether the caller hears what's relayed ([`Occurrence::relayed`]).
    pub relayed: bool,
}

impl Filter {
    /// `arguments` for event type `name`, checked against its input schema.
    pub fn parse(name: Name, arguments: Option<&Value>) -> Result<Filter, ErrorData> {
        let empty = Map::new();
        let args = match arguments {
            None | Some(Value::Null) => &empty,
            Some(Value::Object(m)) => m,
            Some(_) => return Err(invalid("arguments must be an object")),
        };
        let mut f = Filter {
            name: Some(name),
            ..Filter::default()
        };
        for (key, value) in args {
            match key.as_str() {
                "host" | "project" => {
                    let s = value
                        .as_str()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .ok_or_else(|| invalid(format!("{key} must be a name")))?
                        .to_owned();
                    if key == "host" {
                        f.host = Some(s);
                    } else {
                        f.project = Some(s);
                    }
                }
                "agents" => {
                    let refs: Vec<&Value> = match value {
                        Value::String(_) => vec![value],
                        Value::Array(a) if (1..=MAX_FILTER_AGENTS).contains(&a.len()) => {
                            a.iter().collect()
                        }
                        _ => {
                            return Err(invalid(format!(
                                "agents must be a ref, or a list of up to {MAX_FILTER_AGENTS}"
                            )));
                        }
                    };
                    let mut list = Vec::new();
                    for r in refs {
                        let (host, agent) = r
                            .as_str()
                            .map(split_ref)
                            .filter(|(h, a)| !a.is_empty() && h.is_none_or(|h| !h.is_empty()))
                            .ok_or_else(|| {
                                invalid("agents must be refs, like quiet-amber-otter")
                            })?;
                        list.push((host.map(str::to_owned), agent.to_owned()));
                    }
                    f.agents = Some(list);
                }
                "state" if name.has_state() => {
                    let allowed = name.states();
                    let one = |v: &Value| {
                        v.as_str()
                            .filter(|s| allowed.contains(s))
                            .map(str::to_owned)
                            .ok_or_else(|| {
                                invalid(format!("state must be one of {}", allowed.join(", ")))
                            })
                    };
                    let states = match value {
                        Value::Array(a) if !a.is_empty() => {
                            a.iter().map(one).collect::<Result<Vec<_>, _>>()?
                        }
                        v => vec![one(v)?],
                    };
                    f.states = Some(states);
                }
                other => {
                    return Err(invalid(format!(
                        "{} takes no argument {other:?}; it takes {}",
                        name.as_str(),
                        name.input_schema()["properties"]
                            .as_object()
                            .map(|p| p.keys().cloned().collect::<Vec<_>>().join(", "))
                            .unwrap_or_default()
                    )));
                }
            }
        }
        Ok(f)
    }

    pub fn matches(&self, o: &Occurrence) -> bool {
        (!o.relayed || self.relayed)
            && self.name.is_none_or(|n| n == o.name)
            && self.host.as_ref().is_none_or(|h| *h == o.host)
            && self
                .project
                .as_ref()
                .is_none_or(|p| o.project.as_ref() == Some(p))
            && self.agents.as_ref().is_none_or(|list| {
                list.iter()
                    .any(|(h, a)| *a == o.agent && h.as_ref().is_none_or(|h| *h == o.host))
            })
            && self.states.as_ref().is_none_or(|states| {
                o.state
                    .as_ref()
                    .is_some_and(|s| states.iter().any(|x| x == s))
            })
    }

    /// Whether an event on `host` could be one of this filter's.
    fn may_include_host(&self, host: &str) -> bool {
        self.host.as_ref().is_none_or(|h| h == host)
            && self.agents.as_ref().is_none_or(|list| {
                list.iter()
                    .any(|(h, _)| h.as_ref().is_none_or(|h| h == host))
            })
    }
}

// The requests.

/// Whether `method` is one of this extension's.
pub fn is_events_method(method: &str) -> bool {
    method.starts_with("events/")
}

pub(crate) fn params_object(params: Option<&Value>) -> Result<Map<String, Value>, ErrorData> {
    match params {
        None | Some(Value::Null) => Ok(Map::new()),
        Some(Value::Object(m)) => Ok(m.clone()),
        Some(_) => Err(invalid("params must be an object")),
    }
}

/// The event type a request names, which must be one.
pub(crate) fn named(params: &Map<String, Value>) -> Result<Name, ErrorData> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("name is required: an event type from events/list"))?;
    Name::parse(name).ok_or_else(|| {
        error(
            NOT_FOUND,
            format!("no event type called {name}; events/list lists them"),
            Some(json!({"kind": "event"})),
        )
    })
}

pub(crate) fn cursor_param(params: &Map<String, Value>) -> Result<Option<String>, ErrorData> {
    match params.get("cursor") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(invalid("cursor must be a string, or null for now")),
    }
}

/// `events/list`: every event type, in one page.
pub fn list(params: Option<&Value>) -> Result<Value, ErrorData> {
    let params = params_object(params)?;
    if cursor_param(&params)?.is_some() {
        return Err(invalid("events/list has one page, and no cursor"));
    }
    Ok(json!({
        "resultType": "complete",
        "events": Name::ALL.iter().map(|n| n.descriptor()).collect::<Vec<_>>(),
    }))
}

/// Decodes a header value written as SEP-2243 says: as it is, or Base64 in
/// `=?base64?…?=`.
fn header_text(v: &str) -> Option<String> {
    let Some(b64) = v
        .strip_prefix("=?base64?")
        .and_then(|r| r.strip_suffix("?="))
    else {
        return Some(v.to_owned());
    };
    let mut bits = 0u32;
    let mut n = 0;
    let mut out = Vec::new();
    for c in b64.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        bits = (bits << 6) | u32::from(v);
        n += 6;
        if n >= 8 {
            n -= 8;
            out.push((bits >> n) as u8);
            bits &= (1 << n) - 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Checks the `Mcp-Name` header of a request that names an event type: on
/// Streamable HTTP from 2026-07-28 it must say `params.name` (Routing
/// Headers).
pub fn check_name_header(
    headers: &http::HeaderMap,
    method: &str,
    params: Option<&Value>,
) -> Result<(), ErrorData> {
    if method == "events/list" {
        return Ok(());
    }
    let modern = headers
        .get("mcp-protocol-version")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v >= rmcp::model::ProtocolVersion::V_2026_07_28.as_str());
    if !modern {
        return Ok(());
    }
    let want = params.and_then(|p| p.get("name")).and_then(Value::as_str);
    let mut given = headers.get_all("mcp-name").iter();
    let (first, second) = (given.next(), given.next());
    let got = first
        .filter(|_| second.is_none())
        .and_then(|v| v.to_str().ok())
        .and_then(header_text);
    match (want, got) {
        (Some(w), Some(g)) if w == g => Ok(()),
        (want, _) => Err(ErrorData::header_mismatch(
            format!(
                "the Mcp-Name header must be the event type, {}",
                want.unwrap_or("params.name")
            ),
            None,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: EventKind, host: &str, agent: &str, state: Option<&str>) -> Event {
        Event {
            seq: 0,
            at: "2026-11-25T12:00:00Z".into(),
            host: host.into(),
            kind,
            project: Some("app".into()),
            agent: Some(agent.into()),
            state: state.map(str::to_owned),
            limit: None,
            message: None,
            status: None,
        }
    }

    fn all() -> Filter {
        Filter::default()
    }

    fn names(r: &Read) -> Vec<(&'static str, String, Option<String>)> {
        r.events
            .iter()
            .map(|o| {
                (
                    o.name.as_str(),
                    display_ref(&o.host, &o.agent),
                    o.state.clone(),
                )
            })
            .collect()
    }

    #[test]
    fn a_turn_ends_when_busy_stops() {
        let hub = Hub::new();
        hub.ingest(&event(EventKind::AgentAdded, "local", "a", Some("busy")));
        hub.ingest(&event(EventKind::AgentStatus, "local", "a", None));
        hub.ingest(&event(
            EventKind::AgentState,
            "local",
            "a",
            Some("question"),
        ));
        // The same state again isn't a change.
        hub.ingest(&event(
            EventKind::AgentState,
            "local",
            "a",
            Some("question"),
        ));
        hub.ingest(&event(EventKind::AgentState, "local", "a", Some("busy")));
        hub.ingest(&event(EventKind::AgentState, "vps", "b", Some("idle")));
        hub.ingest(&event(EventKind::AgentDeleted, "local", "a", None));
        let r = hub.read(0, &all(), 100);
        assert_eq!(
            names(&r),
            [
                ("agent.started", "a".into(), Some("busy".into())),
                ("agent.state_changed", "a".into(), Some("question".into())),
                ("agent.turn_ended", "a".into(), Some("question".into())),
                ("agent.state_changed", "a".into(), Some("busy".into())),
                ("agent.state_changed", "vps:b".into(), Some("idle".into())),
                ("agent.deleted", "a".into(), None),
            ]
        );
        let previous: Vec<Option<String>> = r
            .events
            .iter()
            .filter(|o| o.name == Name::StateChanged)
            .map(|o| o.previous.clone())
            .collect();
        assert_eq!(
            previous,
            [Some("busy".into()), Some("question".into()), None]
        );
        assert!(!r.has_more && !hub.gap_after(0, &all()));
        assert_eq!(r.cursor, 6);
        // Seeded, a first change has its previous, and ends a turn.
        let hub = Hub::new();
        hub.seed("vps", "b", AgentState::Busy);
        hub.ingest(&event(EventKind::AgentState, "vps", "b", Some("idle")));
        let r = hub.read(0, &all(), 100);
        assert_eq!(
            names(&r),
            [
                ("agent.state_changed", "vps:b".into(), Some("idle".into())),
                ("agent.turn_ended", "vps:b".into(), Some("idle".into())),
            ]
        );
        assert_eq!(r.events.first().unwrap().previous.as_deref(), Some("busy"));
    }

    #[test]
    fn filters_narrow_and_say_what_they_take() {
        let hub = Hub::new();
        for (host, agent, state) in [
            ("local", "a", "busy"),
            ("local", "a", "idle"),
            ("vps", "a", "question"),
            ("vps", "c", "idle"),
        ] {
            hub.ingest(&event(EventKind::AgentState, host, agent, Some(state)));
        }
        let f = |args: Value| Filter::parse(Name::StateChanged, Some(&args)).unwrap();
        let count = |f: &Filter| hub.read(0, f, 100).events.len();
        assert_eq!(count(&f(json!({}))), 4);
        assert_eq!(count(&f(json!({"host": "vps"}))), 2);
        assert_eq!(count(&f(json!({"agents": "a"}))), 3);
        assert_eq!(count(&f(json!({"agents": ["vps:a", "c"]}))), 2);
        assert_eq!(count(&f(json!({"state": ["idle", "question"]}))), 3);
        assert_eq!(count(&f(json!({"project": "web"}))), 0);
        assert_eq!(count(&f(json!({"project": "app", "state": "busy"}))), 1);
        let started = Filter::parse(Name::Started, None).unwrap();
        assert_eq!(count(&started), 0);
        for bad in [
            json!({"colour": "red"}),
            json!({"host": 3}),
            json!({"host": " "}),
            json!({"agents": []}),
            json!({"agents": [""]}),
            json!({"agents": ":x"}),
            json!({"state": "sleeping"}),
            json!([]),
        ] {
            let e = Filter::parse(Name::StateChanged, Some(&bad)).unwrap_err();
            assert_eq!(e.code, ErrorCode::INVALID_PARAMS, "{bad}");
        }
        // A turn never ends busy, and agent.started takes no state.
        assert!(Filter::parse(Name::TurnEnded, Some(&json!({"state": "busy"}))).is_err());
        let e = Filter::parse(Name::Started, Some(&json!({"state": "busy"}))).unwrap_err();
        assert!(e.message.contains("agents, host, project"), "{}", e.message);
    }

    #[test]
    fn cursors_go_on_from_where_they_were() {
        let hub = Hub::new();
        let (start, reset) = hub.resolve(None).unwrap();
        assert_eq!((start, reset), (0, false));
        let now = hub.cursor(hub.last());
        assert_eq!(hub.resolve(Some(&now)).unwrap(), (0, false));
        hub.ingest(&event(EventKind::AgentState, "local", "a", Some("idle")));
        hub.ingest(&event(EventKind::AgentState, "local", "b", Some("idle")));
        let r = hub.read(0, &all(), 1);
        assert_eq!((r.events.len(), r.cursor, r.has_more), (1, 1, true));
        let r = hub.read(r.cursor, &all(), 1);
        assert_eq!((r.events.len(), r.cursor, r.has_more), (1, 2, false));
        // Nothing of a filter's, and the cursor still moves on.
        let none = Filter::parse(Name::Deleted, None).unwrap();
        assert_eq!(hub.read(0, &none, 10).cursor, 2);
        // An earlier vibed's cursor starts again from now, truncated.
        assert_eq!(hub.resolve(Some("0123456789ab-1")).unwrap(), (2, true));
        for bad in ["", "x", "nope-x", &format!("{}-3", hub.boot)] {
            assert!(hub.resolve(Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn gaps_are_said_to_the_subscriptions_they_touch() {
        let hub = Hub::new();
        let link = |state: &str| {
            let mut e = event(EventKind::HostLink, "vps", "", Some(state));
            e.agent = None;
            e
        };
        hub.ingest(&link("connected"));
        // A report while it's up is no gap.
        hub.ingest(&link("connected"));
        hub.ingest(&link("down"));
        hub.ingest(&event(EventKind::AgentState, "local", "a", Some("idle")));
        hub.ingest(&link("connected"));
        // The first coming up (1), and coming back up (3) after the event (2).
        assert_eq!(hub.last(), 3);
        assert!(hub.gap_after(0, &all()));
        assert!(hub.gap_after(2, &all()));
        assert!(!hub.gap_after(3, &all()));
        // Only this host's agents can't have missed anything.
        let here = Filter::parse(Name::StateChanged, Some(&json!({"host": "local"}))).unwrap();
        assert!(!hub.gap_after(0, &here));
        let named = Filter::parse(Name::StateChanged, Some(&json!({"agents": "local:a"}))).unwrap();
        assert!(!hub.gap_after(0, &named));
        let bare = Filter::parse(Name::StateChanged, Some(&json!({"agents": "a"}))).unwrap();
        assert!(hub.gap_after(0, &bare));
        // The hub's own feed falling behind touches every one.
        hub.lagged();
        assert!(hub.gap_after(3, &here));
    }

    #[test]
    fn the_ring_keeps_the_last_and_says_when_it_moved_on() {
        let hub = Hub::new();
        for i in 0..RING + 10 {
            let state = if i % 2 == 0 { "idle" } else { "busy" };
            hub.ingest(&event(EventKind::AgentState, "local", "a", Some(state)));
        }
        let last = lock(&hub.inner).last;
        // Every other change ends a turn, so there are more entries than changes.
        assert_eq!(lock(&hub.inner).ring.len(), RING);
        let r = hub.read(0, &all(), 1);
        assert_eq!(
            r.events[0].seq,
            last - RING as u64 + 1,
            "from what it holds"
        );
        assert!(hub.gap_after(0, &all()));
        let r = hub.read(last - 1, &all(), 10);
        assert!(!hub.gap_after(last - 1, &all()) && r.events.len() == 1);
    }

    #[test]
    fn cursors_resolve_and_gaps_after_them_show() {
        let hub = Hub::new();
        hub.ingest(&event(EventKind::AgentState, "local", "a", Some("idle")));
        assert_eq!(hub.resolve(None).unwrap(), (1, false));
        assert_eq!(hub.resolve(Some(&hub.cursor(1))).unwrap(), (1, false));
        // One from an earlier vibed is now, and truncated.
        assert_eq!(hub.resolve(Some("feedfacecafe-1")).unwrap(), (1, true));
        assert!(hub.resolve(Some(&hub.cursor(9))).is_err());
        assert!(hub.resolve(Some("nonsense")).is_err());
        assert!(!hub.gap_after(0, &all()));
        let mut link = event(EventKind::HostLink, "vps", "", Some("connected"));
        link.agent = None;
        hub.ingest(&link);
        assert!(hub.gap_after(1, &all()));
        assert!(!hub.gap_after(2, &all()), "the gap is behind it");
        let here = Filter::parse(Name::StateChanged, Some(&json!({"host": "local"}))).unwrap();
        assert!(!hub.gap_after(1, &here), "not a host it hears");
        hub.relayed_gap();
        assert!(
            !hub.gap_after(2, &all()),
            "relayed, to a filter that hears none"
        );
        let heard = Filter {
            relayed: true,
            ..Filter::default()
        };
        assert!(hub.gap_after(2, &heard));
    }

    #[test]
    fn four_event_types_listed_with_their_schemas() {
        let l = list(None).unwrap();
        assert_eq!(l["resultType"], "complete");
        let events = l["events"].as_array().unwrap();
        let names: Vec<&str> = events.iter().map(|e| e["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            [
                "agent.started",
                "agent.state_changed",
                "agent.turn_ended",
                "agent.deleted"
            ]
        );
        for e in events {
            assert_eq!(e["delivery"], json!(["webhook"]));
            assert_eq!(e["inputSchema"]["type"], "object");
            assert_eq!(e["inputSchema"]["additionalProperties"], false);
            assert!(
                e["payloadSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("ref"))
            );
            assert!(e["description"].as_str().unwrap().len() > 20);
        }
        assert!(events[1]["inputSchema"]["properties"]["state"].is_object());
        assert!(
            events[0]["inputSchema"]["properties"]
                .get("state")
                .is_none()
        );
        assert!(list(Some(&json!({"cursor": "2"}))).is_err());
    }

    #[test]
    fn name_headers() {
        let mut h = http::HeaderMap::new();
        let p = json!({"name": "agent.deleted"});
        // Before 2026-07-28, nothing's asked of them.
        assert!(check_name_header(&h, "events/subscribe", Some(&p)).is_ok());
        h.insert("mcp-protocol-version", "2026-07-28".parse().unwrap());
        assert!(check_name_header(&h, "events/list", None).is_ok());
        let e = check_name_header(&h, "events/subscribe", Some(&p)).unwrap_err();
        assert_eq!(e.code, ErrorCode::HEADER_MISMATCH);
        h.insert("mcp-name", "agent.started".parse().unwrap());
        assert!(check_name_header(&h, "events/unsubscribe", Some(&p)).is_err());
        h.insert("mcp-name", "agent.deleted".parse().unwrap());
        assert!(check_name_header(&h, "events/unsubscribe", Some(&p)).is_ok());
        h.insert(
            "mcp-name",
            "=?base64?YWdlbnQuZGVsZXRlZA==?=".parse().unwrap(),
        );
        assert!(check_name_header(&h, "events/subscribe", Some(&p)).is_ok());
        h.append("mcp-name", "agent.deleted".parse().unwrap());
        assert!(check_name_header(&h, "events/subscribe", Some(&p)).is_err());
        assert_eq!(header_text("=?base64?w6k=?=").as_deref(), Some("é"));
        assert_eq!(header_text("=?base64?!?="), None);
    }

    #[test]
    fn what_is_relayed_goes_only_to_callers_its_daemon_takes() {
        let hub = Hub::new();
        let heard = Filter {
            relayed: true,
            ..Filter::default()
        };
        hub.ingest(&event(EventKind::AgentAdded, "local", "a", Some("busy")));
        // The relay's view of the Mac now, then what it says from then on.
        hub.seed_relayed("mac", "m", "busy");
        hub.relayed_gap();
        hub.ingest_relayed(&event(EventKind::AgentState, "mac", "m", Some("idle")));
        hub.ingest_relayed(&event(EventKind::AgentAdded, "edge", "e", Some("busy")));
        // Nothing relayed is ever this host's own.
        hub.ingest_relayed(&event(EventKind::AgentAdded, "local", "x", Some("busy")));
        let r = hub.read(0, &heard, 100);
        assert_eq!(
            names(&r),
            [
                ("agent.started", "a".into(), Some("busy".into())),
                ("agent.state_changed", "mac:m".into(), Some("idle".into())),
                ("agent.turn_ended", "mac:m".into(), Some("idle".into())),
                ("agent.started", "edge:e".into(), Some("busy".into())),
            ]
        );
        assert!(
            hub.gap_after(0, &heard),
            "the relay's start is a gap to those who hear it"
        );
        assert_eq!(r.events.get(1).unwrap().previous.as_deref(), Some("busy"));
        // A caller the relaying daemon doesn't take hears none of it, gaps included.
        let r = hub.read(0, &all(), 100);
        assert_eq!(
            names(&r),
            [("agent.started", "a".into(), Some("busy".into()))]
        );
        assert!(!hub.gap_after(0, &all()));
        assert_eq!(r.cursor, 5);
        // A host the relay reaches coming back is a gap for that host alone.
        let at = r.cursor;
        let mut link = event(EventKind::HostLink, "edge", "", Some("connected"));
        link.agent = None;
        hub.ingest_relayed(&link);
        let edge_only = Filter {
            host: Some("edge".into()),
            ..heard.clone()
        };
        let mac_only = Filter {
            host: Some("mac".into()),
            ..heard.clone()
        };
        assert!(hub.gap_after(at, &edge_only));
        assert!(!hub.gap_after(at, &mac_only));
        assert!(!hub.gap_after(at, &all()));
    }
}
