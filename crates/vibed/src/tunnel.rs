//! The Mac's end of the tunnel from a Linux host (PRD: Reaching MCP from elsewhere): the SSH
//! reverse tunnel it keeps to each host linked to it, and the socket each one reaches here.
//!
//! The Mac dials out, so nothing is opened on it: `ssh -R` forwards a Unix socket in the
//! host's private `run/` (see [`crate::mac_peer`]) to a socket of that host's here,
//! `run/t/<host>.sock`. That socket isn't `vibed.sock`: it takes only `daemon.hello` and
//! `tunnel.call`, which runs one of the agent calls MCP makes ([`TUNNEL_METHODS`]), for
//! this Mac's agents or, through it, those of another host linked to it, after checking
//! the caller's API key itself. So a host's MCP reaches the whole of what this Mac does,
//! with no SSH between the hosts. `hosts.list` there says which: this Mac's row and the
//! other hosts', never the asking host's own, or anyone's address. Asking for itself is
//! refused, as that would only come back. A key the host took but this Mac doesn't know,
//! revoked here and not yet there, say, or one of the host's own, gets nothing; so a host
//! that's been taken over can use the tunnel, but can't drive the Mac without a key.
//!
//! Each tunnel is supervised like the link: it waits for the host's link, has the host
//! clear the tunnel's end, starts `ssh`, counts as up once the host has come through it,
//! and starts over with backoff when it ends or the host stops coming through it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use vibe_core::model::{LOCAL_HOST, TunnelState};
use vibe_core::rpc::{ACCEPTS_CALLS, AgentsList, accepted_frame};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Hello, HelloParams, HostsList, Id, Method, NoParams, Request, Response,
    TUNNEL_METHODS, TunnelCall, TunnelCallParams, TunnelPrepare,
};

use crate::core::{Core, error, lock};
use crate::link::{Link, Timing};

/// The oldest a send, start or delete can be on arriving, in seconds. Older, it sat in
/// the tunnel while this Mac slept, and whoever made it has long been told nobody knows
/// how it ended.
const CHANGE_TOO_OLD: i64 = 60;

/// Where each host's socket is, in `run/`: out of reach of a scan for sessions there.
pub const TUNNEL_DIR: &str = "t";

/// Connections the tunnel's socket serves at once: a few more than the host
/// keeps (4), as a slow start holds one.
const MAX_CONNECTIONS: usize = 16;

/// The largest message it takes: the public endpoint's largest request (4
/// MB) wrapped with its key. A host can't make the Mac hold much more.
const MAX_MESSAGE: usize = 5 << 20;

/// How long a connection may sit with nothing asked before it's closed: the
/// host keeps idle ones for 30 s.
const IDLE: Duration = Duration::from_secs(120);

/// How long a call with a key this Mac doesn't know waits for its refusal,
/// so keys can't be tried quickly through the tunnel.
const REFUSED_PAUSE: Duration = Duration::from_secs(1);

/// How recently the host must have come through for the tunnel to count as
/// up: a few of its looks (every 5 s).
pub const SEEN_WITHIN: Duration = Duration::from_secs(20);

/// How long a tunnel must have held for the next to start without backoff.
const HELD: Duration = Duration::from_secs(30);

/// The tunnel to one host, as the Mac keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TunnelView {
    pub state: TunnelState,
    /// Why it's down, or what it waits for.
    pub detail: Option<String>,
}

/// A running tunnel supervisor; dropping it ends the tunnel.
#[derive(Debug)]
pub(crate) struct Tunnel {
    task: JoinHandle<()>,
    view: Arc<Mutex<TunnelView>>,
    /// Serves its socket here, which goes with it, along with its lock.
    serving: JoinHandle<()>,
    socket: PathBuf,
    _guard: vibe_core::socket::SocketGuard,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
        self.serving.abort();
        let _ = std::fs::remove_file(vibe_core::socket::lock_path(&self.socket));
    }
}

/// Whose tunnel a socket here is: the host that's asking, by what only it can reach.
#[derive(Debug)]
pub(crate) struct TunnelEnd {
    host: String,
    /// When that host last came through it.
    seen: Mutex<Option<Instant>>,
}

// The tunnel's socket.

fn reply(id: Option<Id>, outcome: Result<Value, ErrorObject>) -> Option<String> {
    let id = id?;
    let r = match outcome {
        Ok(v) => Response::result(id, v),
        Err(e) => Response::error(id, e),
    };
    Some(serde_json::to_string(&r).expect("responses serialize"))
}

/// Answers one message that came through the tunnel.
pub(crate) async fn tunnel_message(core: &Core, end: &TunnelEnd, line: &[u8]) -> Option<String> {
    let request = match serde_json::from_slice::<Request>(line) {
        Ok(r) => r,
        Err(_) => {
            let e = ErrorObject::new(ErrorCode::InvalidRequest, "Invalid Request");
            return reply(Some(Id::Null), Err(e));
        }
    };
    let outcome = match request.method.as_str() {
        Hello::NAME => hello(core, end, request.params),
        TunnelCall::NAME => tunnel_call(core, end, request.params).await,
        other => Err(ErrorObject::new(
            ErrorCode::MethodNotFound,
            format!("Method not found: {other}: this socket takes daemon.hello and tunnel.call"),
        )),
    };
    reply(request.id, outcome)
}

fn decode<T: serde::de::DeserializeOwned>(params: Option<Value>) -> Result<T, ErrorObject> {
    serde_json::from_value(params.unwrap_or_else(|| json!({})))
        .map_err(|e| ErrorObject::new(ErrorCode::InvalidParams, format!("Invalid params: {e}")))
}

fn hello(core: &Core, end: &TunnelEnd, params: Option<Value>) -> Result<Value, ErrorObject> {
    let p: HelloParams = decode(params)?;
    *lock(&end.seen) = Some(Instant::now());
    let r = crate::dispatch::hello(core.info(), p)?;
    let mut answer = serde_json::to_value(r).expect("hello serializes");
    answer[ACCEPTS_CALLS] = json!(true);
    Ok(answer)
}

async fn tunnel_call(
    core: &Core,
    end: &TunnelEnd,
    params: Option<Value>,
) -> Result<Value, ErrorObject> {
    let p: TunnelCallParams = decode(params)?;
    if !TUNNEL_METHODS.contains(&p.method.as_str()) {
        return Err(ErrorObject::new(
            ErrorCode::MethodNotFound,
            format!(
                "Method not found: {}: the tunnel takes {}",
                p.method,
                TUNNEL_METHODS.join(", ")
            ),
        ));
    }
    let mut params = match p.params {
        None => serde_json::Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "Invalid params: params must be an object",
            ));
        }
    };
    // The key is checked here, where the agents live, whatever the host
    // made of it. A key pushed here (a Mac never has any) doesn't count.
    let key = match core.verify_key(&p.key) {
        Ok(Some(k)) if !k.pushed => k,
        Ok(_) => {
            crate::log!(
                "vibed: tunnel: refused {} with a key this Mac doesn't know",
                p.method
            );
            tokio::time::sleep(REFUSED_PAUSE).await;
            return Err(error(
                ErrorCode::Unauthorized,
                "this Mac doesn't take that API key: it's unknown here, revoked, or a Linux host's own",
            ));
        }
        Err(e) => return Err(e),
    };
    // Only now, so what's linked here is no one's to find out without a key: this Mac, or
    // another host linked to it. Not the one asking, which would only come back to it.
    let through = match params.remove("host") {
        None | Some(Value::Null) => None,
        Some(Value::String(h)) if h == LOCAL_HOST => None,
        Some(Value::String(h)) if h != end.host && core.link(&h).is_some() => Some(h),
        Some(_) => {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "Invalid params: a tunnel reaches this Mac and the other hosts linked to it",
            ));
        }
    };
    crate::log!(
        "vibed: tunnel: key {:?} called {} from {}{}",
        key.name,
        p.method,
        end.host,
        through
            .as_deref()
            .map_or(String::new(), |h| format!(" for {h}"))
    );
    if let Some(host) = &through {
        params.insert("host".into(), json!(host));
    }
    if crate::link::check_after(&p.method).is_some()
        && let Some(age) = p.sent_at.map(|at| core.now().saturating_sub(at))
        && age > CHANGE_TOO_OLD
    {
        crate::log!("vibed: tunnel: refused {}, {age} s old", p.method);
        return Err(error(
            ErrorCode::Conflict,
            format!(
                "{} took {age} s to reach the Mac, so it didn't run. Try again if it's still wanted. If this keeps happening, the clocks on the Mac and this host may disagree",
                p.method
            ),
        ));
    }
    let link = through.as_deref().and_then(|h| core.link(h));
    let mut answer = match &link {
        // What it says now or nothing: the Mac's copy would read as an answer, and the host
        // asking would call its agents available.
        Some(link) if p.method == AgentsList::NAME => {
            let agents = core
                .remote_agents_now(link)
                .await
                .map_err(|e| unavailable_without_address(link, e))?;
            json!({ "agents": agents })
        }
        _ => crate::dispatch::call_here(core, &p.method, Value::Object(params))
            .await
            .map_err(|e| match &link {
                Some(link) => without_address(link, e),
                None => e,
            })?,
    };
    if let Some(link) = &link {
        attach_lines_there(&mut answer, &link.name);
    }
    // Its memory comes with its agents, so a listing needs one call and not two; and which
    // other hosts there are, so a host's MCP keeps up with what's linked here.
    if p.method == AgentsList::NAME {
        let hosts = core.hosts().map(|l| l.hosts).unwrap_or_default();
        let name = through.as_deref().unwrap_or(LOCAL_HOST);
        if let Some(memory) = hosts
            .iter()
            .find(|h| h.name == name)
            .and_then(|h| h.memory.clone())
        {
            answer["memory"] = serde_json::to_value(memory).expect("memory serializes");
        }
        answer["linked_hosts"] = json!(
            hosts
                .iter()
                .filter(|h| h.name != LOCAL_HOST && h.name != end.host)
                .map(|h| h.name.as_str())
                .collect::<Vec<_>>()
        );
    }
    // Not the host that's asking, nor anyone's address.
    if p.method == HostsList::NAME
        && let Some(hosts) = answer.get_mut("hosts").and_then(Value::as_array_mut)
    {
        hosts.retain(|h| h.get("name").and_then(Value::as_str) != Some(end.host.as_str()));
        for row in hosts.iter_mut().filter_map(Value::as_object_mut) {
            for private in [
                "address",
                "detail",
                "tunnel",
                "tunnel_detail",
                "upgrade_error",
            ] {
                row.remove(private);
            }
        }
    }
    Ok(answer)
}

/// What a host's MCP gets of an agent on another host, whose attach line here is over SSH
/// to its address: the line that attaches on that host, which says nothing of where it is.
fn attach_lines_there(value: &mut Value, host: &str) {
    match value {
        Value::Object(map) => {
            if map.contains_key("attach")
                && let Some(agent) = map.get("ref").and_then(Value::as_str)
            {
                let line = format!("on {host}: vibe attach {agent}");
                map.insert("attach".into(), json!(line));
            }
            map.values_mut().for_each(|v| attach_lines_there(v, host));
        }
        Value::Array(items) => items.iter_mut().for_each(|v| attach_lines_there(v, host)),
        _ => {}
    }
}

/// `e`, with what ssh said of `link`'s address put by its name: a link's errors quote it.
fn without_address(link: &Link, mut e: ErrorObject) -> ErrorObject {
    let bare = link
        .address
        .trim_start_matches("ssh://")
        .rsplit('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default();
    e.message = e.message.replace(&link.address, &link.name);
    if bare.contains('.') {
        e.message = e.message.replace(bare, &link.name);
    }
    e
}

/// What a listing gets that couldn't be answered now: always unavailable, whatever went
/// wrong, so what's asking goes by what it last saw.
fn unavailable_without_address(link: &Link, e: ErrorObject) -> ErrorObject {
    let e = without_address(link, e);
    if e.code == ErrorCode::HostUnavailable.code() {
        return e;
    }
    crate::link::unavailable(&link.name, Some(&e.message))
}

/// Serves one connection to the tunnel's socket: one message per line,
/// answered in turn.
async fn connection(core: Arc<Core>, end: Arc<TunnelEnd>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut read = BufReader::new(read);
    let mut line = Vec::new();
    loop {
        line.clear();
        let limit = MAX_MESSAGE as u64 + 1;
        let mut taken = (&mut read).take(limit);
        let next = taken.read_until(b'\n', &mut line);
        match tokio::time::timeout(IDLE, next).await {
            Ok(Ok(0) | Err(_)) | Err(_) => return,
            Ok(Ok(_)) => {}
        }
        if line.last() != Some(&b'\n') {
            // Too large, or cut off: either way, the end of it.
            if line.len() > MAX_MESSAGE {
                let e = ErrorObject::new(ErrorCode::InvalidRequest, "the message is too large");
                if let Some(r) = reply(Some(Id::Null), Err(e)) {
                    let _ = write.write_all(format!("{r}\n").as_bytes()).await;
                }
            }
            return;
        }
        line.pop();
        // At once: a call can run for a long time, and whoever sent it is waiting to
        // hear that this end is here at all.
        if let Some(frame) = accepted_frame(&line, |m| m == TunnelCall::NAME)
            && write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .is_err()
        {
            return;
        }
        if let Some(r) = tunnel_message(&core, &end, &line).await
            && write.write_all(format!("{r}\n").as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Serves a tunnel's socket on `listener` until the task is aborted.
async fn serve_tunnel_socket(core: Weak<Core>, end: Arc<TunnelEnd>, listener: UnixListener) {
    let room = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut failing = false;
    loop {
        let Ok(permit) = Arc::clone(&room).acquire_owned().await else {
            return;
        };
        match listener.accept().await {
            Ok((stream, _)) => {
                failing = false;
                let Some(core) = core.upgrade() else { return };
                let end = Arc::clone(&end);
                tokio::spawn(async move {
                    connection(core, end, stream).await;
                    drop(permit);
                });
            }
            Err(e) => {
                if !failing {
                    crate::log!("vibed: tunnel: can't accept a connection: {e}");
                    failing = true;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

// The tunnel.

/// Whether `path` can be one end of `ssh -R remote:local`: absolute, with
/// nothing `ssh` reads specially in a forward (`:`, `\\`) and no control
/// characters.
fn usable_path(path: &str) -> bool {
    path.starts_with('/')
        && !path
            .chars()
            .any(|c| c == ':' || c == '\\' || c.is_control())
}

/// Whether `path`, which the host gave, is where a host's tunnel ends: its
/// `run/mac.sock`, and nothing else.
fn usable_remote(path: &str) -> bool {
    usable_path(path)
        && path.ends_with(&format!("/run/{}", crate::mac_peer::TUNNEL_END))
        && !path.split('/').any(|part| part == "..")
}

/// How long a tunnel may go without the host coming through it before it's
/// started anew: from when it began, if the host never has, or from when
/// it last did. That's [`SEEN_WITHIN`], after which it no longer counts as
/// up, and 40 s more, for a host that's slow or briefly away. A tunnel
/// whose far end is gone, a socket `run/` lost on the host while `ssh`
/// still runs, has the host never come through again, so it starts over
/// instead of waiting for someone to end `ssh` (issue #45).
pub const QUIET: Duration = SEEN_WITHIN.saturating_add(Duration::from_secs(40));

/// Why a tunnel that began at `began` should start over at `now`, if it
/// should: the host hasn't come through it for `quiet`, since it began or
/// since it was last `seen` coming through (any time before it began was
/// another tunnel's).
fn gone_quiet(
    host: &str,
    began: Instant,
    seen: Option<Instant>,
    now: Instant,
    quiet: Duration,
) -> Option<String> {
    let seen = seen.filter(|&at| at >= began);
    let since = seen.unwrap_or(began);
    if now.saturating_duration_since(since) < quiet {
        return None;
    }
    let secs = quiet.as_secs().max(1);
    Some(match seen {
        None => format!("{host} didn't come through the tunnel in {secs} s"),
        Some(_) => format!("{host} hasn't come through the tunnel for {secs} s"),
    })
}

impl Core {
    /// `host`'s tunnel's socket on this Mac.
    pub fn tunnel_socket(&self, host: &str) -> PathBuf {
        self.layout
            .run_dir()
            .join(TUNNEL_DIR)
            .join(format!("{host}.sock"))
    }

    /// Serves `host`'s socket (private, with a lock beside it, as `vibed.sock` is).
    fn open_tunnel_socket(
        &self,
        host: &str,
        end: Arc<TunnelEnd>,
        me: &Weak<Core>,
        rt: &tokio::runtime::Handle,
    ) -> Option<(JoinHandle<()>, vibe_core::socket::SocketGuard, PathBuf)> {
        use std::os::unix::fs::DirBuilderExt;
        let path = self.tunnel_socket(host);
        let dir = self.layout.run_dir().join(TUNNEL_DIR);
        if let Err(e) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .and_then(|()| vibe_core::paths::private_dir(&dir))
        {
            crate::log!(
                "vibed: {host}'s MCP can't reach this Mac: {}: {e}",
                dir.display()
            );
            return None;
        }
        let bound = vibe_core::socket::bind(&path).and_then(|(listener, guard)| {
            let io = |source| vibe_core::socket::BindError::Io {
                what: "serve",
                path: path.clone(),
                source,
            };
            listener.set_nonblocking(true).map_err(io)?;
            let _in = rt.enter();
            let listener = UnixListener::from_std(listener).map_err(io)?;
            Ok((listener, guard))
        });
        match bound {
            Ok((listener, guard)) => {
                let task = rt.spawn(serve_tunnel_socket(me.clone(), end, listener));
                Some((task, guard, path))
            }
            Err(e) => {
                crate::log!("vibed: {host}'s MCP can't reach this Mac: {e}");
                None
            }
        }
    }

    /// How `host`'s tunnel is, if this Mac keeps one to it.
    pub(crate) fn tunnel_view(&self, host: &str) -> Option<TunnelView> {
        lock(&self.tunnels).get(host).map(|t| lock(&t.view).clone())
    }

    /// Keeps a tunnel to every Linux host linked here, so each one's MCP reaches this Mac,
    /// and none to one that isn't. Not on a Linux host, which is the far end of one.
    pub(crate) fn sync_tunnel(&self) {
        if self.serves_mac_peer {
            return;
        }
        let (Some(me), Some(rt)) = (self.me.get(), self.runtime.get()) else {
            return;
        };
        let want: Vec<String> = self.store().hosts().map_or(Vec::new(), |hosts| {
            hosts
                .into_iter()
                .filter(|h| h.address.is_some())
                .map(|h| h.name)
                .collect()
        });
        let mut tunnels = lock(&self.tunnels);
        tunnels.retain(|host, _| {
            let keep = want.contains(host);
            if !keep {
                crate::log!("vibed: closing the tunnel to {host}");
            }
            keep
        });
        for name in want {
            if tunnels.contains_key(&name) {
                continue;
            }
            let Some(link) = self.link(&name) else {
                continue;
            };
            let end = Arc::new(TunnelEnd {
                host: name.clone(),
                seen: Mutex::new(None),
            });
            let Some((serving, guard, socket)) =
                self.open_tunnel_socket(&name, Arc::clone(&end), me, rt)
            else {
                continue;
            };
            let view = Arc::new(Mutex::new(TunnelView {
                state: TunnelState::Connecting,
                detail: None,
            }));
            let task = rt.spawn(supervise(
                me.clone(),
                link,
                end,
                Arc::clone(&view),
                self.link_timing,
            ));
            tunnels.insert(
                name,
                Tunnel {
                    task,
                    view,
                    serving,
                    socket,
                    _guard: guard,
                },
            );
        }
    }

    /// Starts what the Mac's tunnels and a host's look at them need, as `vibed` does.
    pub fn spawn_tunnels(self: &Arc<Self>) {
        let rt = tokio::runtime::Handle::current();
        let _ = self.runtime.set(rt.clone());
        let _ = self.me.set(Arc::downgrade(self));
        self.spawn_mac_probe(Arc::downgrade(self), &rt);
        self.sync_tunnel();
    }

    /// Stops them, as `vibed` does when it stops.
    pub fn stop_tunnels(&self) {
        self.stop_mac_probe();
        lock(&self.tunnels).clear();
    }

    /// Closes `host`'s tunnel and clears its end there, so a host this Mac is forgetting has
    /// no `mac` left to list. It's best done with: a host that can't be told keeps the file,
    /// and says the Mac's unavailable, which is true.
    pub(crate) async fn close_tunnel_to(&self, link: &Link) {
        lock(&self.tunnels).remove(&link.name);
        if link.connected()
            && let Err(e) = self
                .forward_typed::<TunnelPrepare>(link, &NoParams {})
                .await
        {
            crate::log!(
                "vibed: can't clear the tunnel's end on {}: {}",
                link.name,
                e.message
            );
        }
    }
}

/// Keeps the tunnel to `link`'s host up, for as long as the core and the
/// supervisor are there.
async fn supervise(
    core: Weak<Core>,
    link: Arc<Link>,
    end: Arc<TunnelEnd>,
    view: Arc<Mutex<TunnelView>>,
    timing: Timing,
) {
    let came_through = |since: Instant| {
        lock(&end.seen).is_some_and(|at| at >= since && at.elapsed() < SEEN_WITHIN)
    };
    let set = |core: &Core, state: TunnelState, detail: Option<String>| {
        let now = TunnelView { state, detail };
        let changed = {
            let mut v = lock(&view);
            let changed = *v != now;
            *v = now.clone();
            changed
        };
        if changed {
            match (&now.state, &now.detail) {
                (TunnelState::Up, _) => crate::log!("vibed: the tunnel to {} is up", link.name),
                (TunnelState::Down, Some(why)) => {
                    crate::log!("vibed: the tunnel to {} is down: {why}", link.name)
                }
                _ => {}
            }
            core.host_link_changed(&link);
        }
    };
    let mut backoff = timing.min_backoff;
    loop {
        let Some(c) = core.upgrade() else { return };
        if !link.connected() {
            set(
                &c,
                TunnelState::Down,
                Some(format!("waiting for {}'s link", link.name)),
            );
            drop(c);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }
        let started = start(&c, &link).await;
        let mut tunnel = match started {
            Ok(t) => t,
            Err(why) => {
                set(&c, TunnelState::Down, Some(why));
                drop(c);
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(timing.max_backoff);
                continue;
            }
        };
        let began = Instant::now();
        set(&c, TunnelState::Connecting, None);
        drop(c);
        let mut was_up = false;
        let why = loop {
            tokio::select! {
                why = &mut tunnel.ended => break why,
                () = tokio::time::sleep(Duration::from_millis(500)) => {
                    let Some(c) = core.upgrade() else { return };
                    let up = came_through(began);
                    was_up |= up;
                    // `ssh` runs, but the host hasn't come through, or has
                    // stopped: start over.
                    let seen = *lock(&end.seen);
                    let quiet = gone_quiet(&link.name, began, seen, Instant::now(), timing.tunnel_quiet);
                    if let Some(why) = quiet {
                        break why;
                    }
                    set(&c, if up { TunnelState::Up } else { TunnelState::Connecting }, None);
                }
            }
        };
        let Some(c) = core.upgrade() else { return };
        set(&c, TunnelState::Down, Some(why));
        drop(c);
        if was_up && began.elapsed() >= HELD {
            backoff = timing.min_backoff;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(timing.max_backoff);
    }
}

/// Has the host clear the tunnel's end, and starts `ssh -R` to it.
async fn start(core: &Core, link: &Link) -> Result<crate::remote::TunnelChild, String> {
    let prepared = core
        .forward_typed::<TunnelPrepare>(link, &NoParams {})
        .await
        .map_err(|e| e.message)?;
    if !usable_remote(&prepared.socket) {
        return Err(format!(
            "{} can't end an ssh tunnel at {}",
            link.name, prepared.socket
        ));
    }
    let local = core.tunnel_socket(&link.name);
    if !local.to_str().is_some_and(usable_path) {
        return Err(format!(
            "{} can't end an ssh tunnel here: its path has a colon",
            local.display()
        ));
    }
    {
        use std::os::unix::fs::FileTypeExt;
        let served = std::fs::symlink_metadata(&local).is_ok_and(|m| m.file_type().is_socket());
        if !served {
            return Err(format!(
                "this Mac's tunnel socket, {}, isn't served: see vibed's log",
                local.display()
            ));
        }
    }
    core.remote
        .tunnel(&link.address, &prepared.socket, Path::new(&local))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tunnel_the_host_stops_coming_through_starts_over() {
        let quiet = Duration::from_secs(60);
        let last_tunnel = Instant::now();
        let began = last_tunnel + Duration::from_secs(10);
        let at = |s: u64| began + Duration::from_secs(s);
        // Never came through: given until `quiet` from the start.
        assert_eq!(gone_quiet("vps", began, None, at(59), quiet), None);
        assert_eq!(
            gone_quiet("vps", began, None, at(60), quiet).as_deref(),
            Some("vps didn't come through the tunnel in 60 s")
        );
        // Came through for a while, then stopped (its socket on the host
        // gone, say): `quiet` from the last time, not from the start.
        assert_eq!(
            gone_quiet("vps", began, Some(at(100)), at(159), quiet),
            None
        );
        assert_eq!(
            gone_quiet("vps", began, Some(at(100)), at(160), quiet).as_deref(),
            Some("vps hasn't come through the tunnel for 60 s")
        );
        // Coming through all along, however long it's been up.
        assert_eq!(
            gone_quiet("vps", began, Some(at(3600)), at(3605), quiet),
            None
        );
        // Seen before this tunnel began was the last tunnel: it doesn't count.
        assert_eq!(
            gone_quiet("vps", began, Some(last_tunnel), at(59), quiet),
            None
        );
        assert!(
            gone_quiet("vps", began, Some(last_tunnel), at(60), quiet)
                .unwrap()
                .contains("didn't come through")
        );
        // By default, a while past when it stops counting as up.
        assert!(QUIET >= SEEN_WITHIN + Duration::from_secs(30));
        assert!(QUIET <= SEEN_WITHIN + Duration::from_secs(60));
        assert_eq!(Timing::default().tunnel_quiet, QUIET);
    }

    #[test]
    fn tunnel_ends_are_plain_absolute_paths() {
        assert!(usable_path("/home/me/.local/share/vibeos/run/mac.sock"));
        assert!(usable_path(
            "/Users/me/Library/Application Support/VibeOS/run/tunnel.sock"
        ));
        assert!(!usable_path("relative/mac.sock"));
        assert!(!usable_path("/a:b/mac.sock"));
        assert!(!usable_path("/a\n/mac.sock"));
        assert!(!usable_path("/a\\/mac.sock"));
        // What a host gives for its end must be its run/mac.sock.
        assert!(usable_remote("/home/me/.local/share/vibeos/run/mac.sock"));
        assert!(!usable_remote("/home/me/.ssh/authorized_keys"));
        assert!(!usable_remote("/run/../tmp/run/mac.sock"));
        assert!(!usable_remote("/home/me/run/mac.sock\\"));
    }
}
