//! The Mac's end of external access (PRD: External access): the SSH reverse
//! tunnel it keeps to the Linux host that's the external address, and the
//! socket that tunnel reaches here.
//!
//! The Mac dials out, so nothing is opened on it: `ssh -R` forwards a Unix
//! socket in the host's private `run/` (see [`crate::external`]) to this
//! Mac's tunnel socket, `run/tunnel.sock`. That socket isn't `vibed.sock`:
//! it takes only `daemon.hello` and `tunnel.call`, which runs one of the
//! agent calls MCP makes ([`TUNNEL_METHODS`]), for this Mac's own agents,
//! after checking the outside caller's API key itself. A key the host took
//! but this Mac doesn't know, revoked here and not yet there, say, or one
//! of the host's own, gets nothing; so a host that's been taken over can
//! use the tunnel, but can't drive the Mac without a key.
//!
//! The tunnel is supervised like the link: it waits for the host's link,
//! has the host clear the tunnel's end, starts `ssh`, counts as up once the
//! host has come through it, and starts over with backoff when it ends or
//! the host stops coming through it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::task::JoinHandle;
use vibe_core::model::{LOCAL_HOST, TunnelState};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Hello, HelloParams, Id, Method, NoParams, Request, Response,
    TUNNEL_METHODS, TunnelCall, TunnelCallParams, TunnelPrepare,
};

use crate::core::{Core, error, lock};
use crate::link::{Link, Timing};

/// The tunnel's socket, in `run/`.
pub const TUNNEL_SOCKET: &str = "tunnel.sock";

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
    pub host: String,
    task: JoinHandle<()>,
    view: Arc<Mutex<TunnelView>>,
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
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
pub async fn tunnel_message(core: &Core, line: &[u8]) -> Option<String> {
    let request = match serde_json::from_slice::<Request>(line) {
        Ok(r) => r,
        Err(_) => {
            let e = ErrorObject::new(ErrorCode::InvalidRequest, "Invalid Request");
            return reply(Some(Id::Null), Err(e));
        }
    };
    let outcome = match request.method.as_str() {
        Hello::NAME => hello(core, request.params),
        TunnelCall::NAME => tunnel_call(core, request.params).await,
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

fn hello(core: &Core, params: Option<Value>) -> Result<Value, ErrorObject> {
    let p: HelloParams = decode(params)?;
    *lock(&core.tunnel_seen) = Some(Instant::now());
    let r = crate::dispatch::hello(core.info(), p)?;
    Ok(serde_json::to_value(r).expect("hello serializes"))
}

async fn tunnel_call(core: &Core, params: Option<Value>) -> Result<Value, ErrorObject> {
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
    // This Mac's own agents only: not those of hosts linked to it.
    match params.remove("host") {
        None | Some(Value::Null) => {}
        Some(Value::String(h)) if h == LOCAL_HOST => {}
        Some(_) => {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "Invalid params: the tunnel reaches this Mac's own agents only",
            ));
        }
    }
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
    crate::log!("vibed: tunnel: key {:?} called {}", key.name, p.method);
    crate::dispatch::call_here(core, &p.method, Value::Object(params)).await
}

/// Serves one connection to the tunnel's socket: one message per line,
/// answered in turn.
async fn connection(core: Arc<Core>, stream: UnixStream) {
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
        if let Some(r) = tunnel_message(&core, &line).await
            && write.write_all(format!("{r}\n").as_bytes()).await.is_err()
        {
            return;
        }
    }
}

/// Serves the tunnel's socket on `listener` until the task is aborted.
pub async fn serve_tunnel_socket(core: Weak<Core>, listener: UnixListener) {
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
                tokio::spawn(async move {
                    connection(core, stream).await;
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
        && path.ends_with(&format!("/run/{}", crate::external::TUNNEL_END))
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
    /// The tunnel's socket on this Mac.
    pub fn tunnel_socket(&self) -> PathBuf {
        self.layout.run_dir().join(TUNNEL_SOCKET)
    }

    /// Serves the tunnel's socket (private, with a lock beside it, as
    /// `vibed.sock` is), unless it's served already. It's there only while
    /// a host is the external address.
    fn open_tunnel_socket(&self, me: &Weak<Core>, rt: &tokio::runtime::Handle) {
        let mut served = lock(&self.tunnel_socket_served);
        if served.is_some() {
            return;
        }
        let path = self.tunnel_socket();
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
                let task = rt.spawn(serve_tunnel_socket(me.clone(), listener));
                *served = Some((task, guard));
            }
            Err(e) => crate::log!("vibed: the external address can't reach this Mac: {e}"),
        }
    }

    /// Stops serving the tunnel's socket, and removes it and its lock.
    pub(crate) fn close_tunnel_socket(&self) {
        if let Some((task, guard)) = lock(&self.tunnel_socket_served).take() {
            task.abort();
            drop(guard);
            let _ = std::fs::remove_file(vibe_core::socket::lock_path(&self.tunnel_socket()));
        }
    }

    /// The tunnel to `host`, if this Mac keeps one to it.
    pub(crate) fn tunnel_view(&self, host: &str) -> Option<TunnelView> {
        let t = lock(&self.tunnel);
        let t = t.as_ref().filter(|t| t.host == host)?;
        Some(lock(&t.view).clone())
    }

    /// Keeps a tunnel to the Linux host the registry says is the external
    /// address, and none to any other. Nothing until this core runs its
    /// background tasks (see [`Core::spawn_external`]).
    pub(crate) fn sync_tunnel(&self) {
        let want = self.store().hosts().ok().and_then(|hosts| {
            hosts
                .into_iter()
                .find(|h| h.external && h.address.is_some())
                .map(|h| h.name)
        });
        let mut tunnel = lock(&self.tunnel);
        if tunnel.as_ref().map(|t| &t.host) == want.as_ref() {
            return;
        }
        if let Some(old) = tunnel.take() {
            crate::log!("vibed: closing the tunnel to {}", old.host);
        }
        let Some(name) = want else {
            self.close_tunnel_socket();
            return;
        };
        let (Some(me), Some(rt)) = (self.me.get(), self.runtime.get()) else {
            return;
        };
        let Some(link) = self.link(&name) else { return };
        self.open_tunnel_socket(me, rt);
        let view = Arc::new(Mutex::new(TunnelView {
            state: TunnelState::Connecting,
            detail: None,
        }));
        let task = rt.spawn(supervise(
            me.clone(),
            link,
            Arc::clone(&view),
            self.link_timing,
        ));
        *tunnel = Some(Tunnel {
            host: name,
            task,
            view,
        });
    }

    /// Whether the host has come through the tunnel since `since`, and
    /// lately.
    fn host_came_through(&self, since: Instant) -> bool {
        lock(&self.tunnel_seen).is_some_and(|at| at >= since && at.elapsed() < SEEN_WITHIN)
    }
}

/// Keeps the tunnel to `link`'s host up, for as long as the core and the
/// supervisor are there.
async fn supervise(
    core: Weak<Core>,
    link: Arc<Link>,
    view: Arc<Mutex<TunnelView>>,
    timing: Timing,
) {
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
                    let up = c.host_came_through(began);
                    was_up |= up;
                    // `ssh` runs, but the host hasn't come through, or has
                    // stopped: start over.
                    let seen = *lock(&c.tunnel_seen);
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
    let local = core.tunnel_socket();
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
