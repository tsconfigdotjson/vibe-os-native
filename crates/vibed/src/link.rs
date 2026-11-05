//! The link to a Linux host (PRD: The link between the Mac and a host). This
//! `vibed` keeps one channel open to the host's `vibed` for its events, and
//! opens more as calls need them, all through [`Remote::dial`], which is
//! `ssh <host> vibe rpc`. It supervises the link: it pings the host, notices
//! when it stops answering, reconnects with backoff, and says whether the
//! link is connected or down. While it's down the host's agents are
//! unavailable here; nothing on either side stops.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;
use vibe_core::model::{HostLink, LOCAL_HOST};
use vibe_core::rpc::{ACCEPTS_CALLS, TUNNEL_METHODS};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, Event, Hello, HostInfo, HostList, HostsList, Method, PROTOCOL_VERSION,
};

use crate::core::{Core, error, lock};
use crate::remote::{ACCEPT_WITHIN, Channel};

/// How often the host is pinged (with `hosts.list`, whose answer also says
/// whether it serves MCP).
pub const PING_EVERY: Duration = Duration::from_secs(10);

/// How long a ping may go unanswered before the link counts as down.
pub const PING_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a new channel gets to answer its hello.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The first wait before reconnecting, and the longest.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Channels kept open between calls, and for how long: less than the host
/// lets a quiet link's connection stay ([`crate::dispatch::LINK_QUIET`]).
const MAX_IDLE: usize = 4;
const IDLE_FOR: Duration = Duration::from_secs(30);

/// What a channel for calls says it is, in its hello.
const LINK_CALLS: &str = "vibed link calls from";

/// How long after one try a host's `vibed` is upgraded by itself again, as long as
/// its link is up or trying: often enough to come right soon after what was wrong
/// is, and rarely enough that it's no load on either side.
const UPGRADE_RETRY: Duration = Duration::from_secs(60 * 60);

/// How long between tries at the least, for a link that keeps coming back.
const UPGRADE_GAP: Duration = Duration::from_secs(5 * 60);

/// How upgrading a host's `vibed` stands, on this Mac, for this run.
#[derive(Debug, Default)]
pub(crate) struct Upgrade {
    /// One's under way or asked for.
    pub(crate) running: bool,
    /// When the last one began, however it went: nothing short of that holds a
    /// host from being installed on over and over.
    pub(crate) tried_at: Option<Instant>,
    /// A link's come up since then. A try the network cut short is no reason to wait
    /// out the hour once it's back, only [`Timing::upgrade_gap`].
    pub(crate) reconnected: bool,
    /// What went wrong the last time, until something's gone right.
    pub(crate) error: Option<String>,
    /// The version it went back to, when the last try's build wouldn't start. That comes
    /// up looking as if nothing is wrong, though this Mac's build is what's wrong, so
    /// it's only another version, or a try that works, that says it's been put right.
    pub(crate) went_back_to: Option<String>,
}

impl Upgrade {
    /// Whether it's too soon for another by itself.
    pub(crate) fn too_soon(&self, timing: &Timing) -> bool {
        let wait = if self.reconnected {
            timing.upgrade_gap
        } else {
            timing.upgrade_retry
        };
        self.tried_at.is_some_and(|at| at.elapsed() < wait)
    }
}

/// How the link is timed, and the tunnel that follows it; tests make it
/// quicker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub ping_every: Duration,
    pub ping_timeout: Duration,
    pub connect_timeout: Duration,
    pub min_backoff: Duration,
    pub max_backoff: Duration,
    /// How long a tunnel may go without the host coming through it, since
    /// it began or since the host last did, before it's started anew
    /// ([`crate::tunnel::QUIET`]).
    pub tunnel_quiet: Duration,
    /// How long after one try the host's `vibed` is upgraded by itself again.
    pub upgrade_retry: Duration,
    /// How long, at the least, if its link has come back since.
    pub upgrade_gap: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            ping_every: PING_EVERY,
            ping_timeout: PING_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            min_backoff: MIN_BACKOFF,
            max_backoff: MAX_BACKOFF,
            tunnel_quiet: crate::tunnel::QUIET,
            upgrade_retry: UPGRADE_RETRY,
            upgrade_gap: UPGRADE_GAP,
        }
    }
}

/// What the link knows now.
#[derive(Debug, Clone, Default)]
struct Status {
    connected: bool,
    /// Why it's down, when it is.
    detail: Option<String>,
    /// What the host last said of itself, its own row of `hosts.list`:
    /// whether it serves MCP, and how its external address is.
    own: Option<HostInfo>,
}

/// One Linux host's link.
pub struct Link {
    pub name: String,
    pub address: String,
    status: Mutex<Status>,
    /// Whether it's connected, for calls to give up on when it goes down.
    up: watch::Sender<bool>,
    idle: Mutex<Vec<(Channel, Instant)>>,
    /// Wakes the supervisor from its backoff, to reconnect now.
    kick: Notify,
    task: Mutex<Option<JoinHandle<()>>>,
    /// How upgrading the host's `vibed` stands.
    pub(crate) upgrade: Mutex<Upgrade>,
    /// One install at a time, whoever asked.
    pub(crate) installing: tokio::sync::Mutex<()>,
    /// It runs an older `vibed` than this one, as of when its link last came up.
    pub(crate) behind: AtomicBool,
    /// A read of its agents is wanted: one runs at a time.
    pub(crate) refresh_wanted: AtomicBool,
    /// It hasn't had this host's latest keys, as far as this `vibed` knows: the last
    /// push failed or couldn't be made, or none has been since it started. The next
    /// ping tries again.
    pub(crate) keys_stale: AtomicBool,
    /// When this Mac last told the host about its external address: a ping sent
    /// before that may answer with what it was.
    /// One push of keys to it at a time, so that an older list can't arrive after a
    /// newer one and undo it, which could bring back a revoked key.
    pub(crate) keys_pushing: tokio::sync::Mutex<()>,
    pub(crate) external_told: Mutex<Option<Instant>>,
    /// Why the host wouldn't take what this Mac last told it, if it wouldn't.
    pub(crate) external_refused: Mutex<Option<String>>,
    /// The projects it last listed, as it still has them while it's away.
    pub(crate) projects_seen: Mutex<Vec<String>>,
    /// Whether its `vibed` sends [`vibe_core::rpc::CALL_ACCEPTED`].
    accepts_calls: AtomicBool,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("name", &self.name)
            .field("address", &self.address)
            .field("status", &*lock(&self.status))
            .finish_non_exhaustive()
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}

/// What a call gets when the host can't be reached.
pub fn unavailable(name: &str, why: Option<&str>) -> ErrorObject {
    error(
        ErrorCode::HostUnavailable,
        match why {
            Some(why) => format!("{name} is unavailable: {why}"),
            None => format!("{name} is unavailable: its link is down"),
        },
    )
}

/// What a call gets that went out over a link and hasn't been heard of since.
fn lost(name: &str, method: &str, why: Option<&str>) -> ErrorObject {
    let e = unavailable(name, why);
    match crate::external::check_after(method) {
        Some(check) => ErrorObject {
            message: format!("{}. Outcome unknown: {check}", e.message),
            ..e
        }
        .outcome_unknown(),
        None => e,
    }
}

impl Link {
    pub fn new(name: &str, address: &str) -> Link {
        Link {
            name: name.to_owned(),
            address: address.to_owned(),
            status: Mutex::new(Status {
                connected: false,
                detail: Some("not connected yet".into()),
                own: None,
            }),
            up: watch::channel(false).0,
            idle: Mutex::new(Vec::new()),
            kick: Notify::new(),
            task: Mutex::new(None),
            upgrade: Mutex::new(Upgrade::default()),
            installing: tokio::sync::Mutex::new(()),
            behind: AtomicBool::new(false),
            refresh_wanted: AtomicBool::new(false),
            keys_stale: AtomicBool::new(true),
            keys_pushing: tokio::sync::Mutex::new(()),
            external_told: Mutex::new(None),
            external_refused: Mutex::new(None),
            projects_seen: Mutex::new(Vec::new()),
            accepts_calls: AtomicBool::new(false),
        }
    }

    pub fn connected(&self) -> bool {
        lock(&self.status).connected
    }

    /// `info` with what the link knows: its state, why it's down, and
    /// whether the host serves MCP.
    pub fn describe(&self, mut info: HostInfo) -> HostInfo {
        let s = lock(&self.status);
        info.link = if s.connected {
            HostLink::Connected
        } else {
            HostLink::Down
        };
        info.detail = if s.connected { None } else { s.detail.clone() };
        info.mcp = s.connected && s.own.as_ref().is_some_and(|o| o.mcp);
        info
    }

    /// What the host last said of itself while its link was up.
    pub fn reported(&self) -> Option<HostInfo> {
        let s = lock(&self.status);
        s.connected.then(|| s.own.clone()).flatten()
    }

    /// The public name the host says it serves the external address at, if
    /// it says it does.
    pub fn reported_domain(&self) -> Option<String> {
        let own = self.reported()?;
        own.external.then_some(())?;
        own.url?
            .strip_prefix("https://")?
            .strip_suffix(vibe_mcp::PATH)
            .map(str::to_owned)
    }

    /// Reconnects now rather than at the end of the backoff.
    pub fn kick(&self) {
        self.kick.notify_one();
    }

    /// Ends the link for good.
    pub fn stop(&self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
        lock(&self.idle).clear();
        self.set_down(Some("removed".into()));
    }

    /// Notes what the host just answered of itself, as `hosts.external`
    /// does, under the name it calls itself.
    pub(crate) fn note_report(&self, mut own: HostInfo) {
        own.name = LOCAL_HOST.to_owned();
        self.set_report(own);
    }

    /// Notes what the host says of itself, and returns whether that
    /// changed. Its memory changes with every ping, so it's kept but isn't
    /// a change: `hosts.list` reads it, and no `host.link` event is due.
    fn set_report(&self, own: HostInfo) -> bool {
        let mut s = lock(&self.status);
        let steady = |h: &HostInfo| HostInfo {
            memory: None,
            ..h.clone()
        };
        let changed = s.connected && s.own.as_ref().map(steady) != Some(steady(&own));
        if s.connected {
            s.own = Some(own);
        }
        changed
    }

    fn set_up(&self, own: Option<HostInfo>) {
        *lock(&self.status) = Status {
            connected: true,
            detail: None,
            own,
        };
        self.up.send_replace(true);
    }

    /// Marks it down, and returns whether that changed anything.
    fn set_down(&self, detail: Option<String>) -> bool {
        let changed = {
            let mut s = lock(&self.status);
            let changed = s.connected || s.detail != detail;
            *s = Status {
                connected: false,
                detail,
                own: None,
            };
            changed
        };
        self.up.send_replace(false);
        lock(&self.idle).clear();
        // What the next link says is news, however soon that is.
        *lock(&self.external_told) = None;
        changed
    }

    /// Calls `method` on the host, over a channel of its own, so a slow
    /// call doesn't hold up the others. It gives up after `timeout`, or as
    /// soon as the link goes down.
    pub async fn call(
        &self,
        core: &Core,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ErrorObject> {
        if !self.connected() {
            let detail = lock(&self.status).detail.clone();
            return Err(unavailable(&self.name, detail.as_deref()));
        }
        // One kept too long may have been closed by the host as quiet.
        let idle = {
            let mut idle = lock(&self.idle);
            idle.retain(|(_, since)| since.elapsed() < IDLE_FOR);
            idle.pop()
        };
        let (mut channel, fresh) = match idle {
            Some((c, _)) => (c, false),
            None => (
                core.remote
                    .dial(&self.address)
                    .await
                    .map_err(|e| unavailable(&self.name, Some(&e)))?,
                true,
            ),
        };
        let mut up = self.up.subscribe();
        let went_down = async move {
            while *up.borrow_and_update() {
                if up.changed().await.is_err() {
                    break;
                }
            }
        };
        let exchange = async {
            // A new channel says it's a link's, so the host closes it once
            // it's quiet, as it would be left by a pulled network.
            if fresh {
                let hello = channel
                    .call(
                        Hello::NAME,
                        json!({"client": format!("{LINK_CALLS} {}", core.label()), "protocol": PROTOCOL_VERSION}),
                    )
                    .await?
                    .map_err(|e| std::io::Error::other(e.message))?;
                self.accepts_calls.store(
                    hello.get(ACCEPTS_CALLS) == Some(&Value::Bool(true)),
                    Ordering::SeqCst,
                );
            }
            // Only the agent calls are said to be taken. Others, as a clone in
            // `projects.add` is, take their time with no word till they're done.
            if self.accepts_calls.load(Ordering::SeqCst) && TUNNEL_METHODS.contains(&method) {
                channel.call_accepted(method, params, ACCEPT_WITHIN).await
            } else {
                channel.call(method, params).await
            }
        };
        let answer = tokio::select! {
            a = tokio::time::timeout(timeout, exchange) => a,
            () = went_down => {
                let detail = lock(&self.status).detail.clone();
                return Err(lost(&self.name, method, detail.as_deref()));
            }
        };
        match answer {
            Ok(Ok(outcome)) => {
                let mut idle = lock(&self.idle);
                if idle.len() < MAX_IDLE && self.connected() {
                    idle.push((channel, Instant::now()));
                }
                outcome
            }
            Ok(Err(e)) => Err(lost(&self.name, method, Some(&e.to_string()))),
            Err(_) => Err(lost(
                &self.name,
                method,
                Some(&format!(
                    "it didn't answer {method} in {} s",
                    timeout.as_secs()
                )),
            )),
        }
    }
}

impl Core {
    /// Runs `task` in the background, on the links' runtime, with this core;
    /// nothing, unless links run (see [`Core::spawn_links`]).
    pub(crate) fn in_background<F>(&self, task: impl FnOnce(Arc<Core>) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let (Some(core), Some(rt)) = (self.me.get().and_then(Weak::upgrade), self.runtime.get())
        else {
            return;
        };
        rt.spawn(task(core));
    }

    /// Starts the link to `link`'s host, unless this core isn't running
    /// links (see [`Core::spawn_links`]).
    pub(crate) fn start_link(&self, link: &Arc<Link>) {
        let (Some(core), Some(rt)) = (self.me.get().cloned(), self.runtime.get()) else {
            return;
        };
        let task = rt.spawn(supervise(core, Arc::downgrade(link), self.link_timing));
        if let Some(old) = lock(&link.task).replace(task) {
            old.abort();
        }
    }

    /// Ends every link, as `vibed` does when it stops: each `ssh` it ran
    /// goes with it. The hosts and their agents run on.
    pub fn stop_links(&self) {
        let links: Vec<Arc<Link>> = lock(&self.links).values().cloned().collect();
        for link in links {
            if let Some(task) = lock(&link.task).take() {
                task.abort();
            }
            lock(&link.idle).clear();
        }
    }

    /// Starts every Linux host's link, and from now on each one added. It
    /// must be called on the runtime they're to run on.
    pub fn spawn_links(self: &Arc<Self>) {
        let _ = self.runtime.set(tokio::runtime::Handle::current());
        let _ = self.me.set(Arc::downgrade(self));
        let links: Vec<Arc<Link>> = lock(&self.links).values().cloned().collect();
        for link in links {
            self.start_link(&link);
        }
    }
}

/// Keeps `link` connected, for as long as the core and the link are there.
async fn supervise(core: Weak<Core>, link: Weak<Link>, timing: Timing) {
    let mut backoff = timing.min_backoff;
    loop {
        let Some(l) = link.upgrade() else {
            return;
        };
        let began = Instant::now();
        let ended = session(&core, &l, timing).await;
        let Some(c) = core.upgrade() else {
            return;
        };
        let why = match &ended {
            Ok(()) => "the link closed".to_owned(),
            Err(why) => why.clone(),
        };
        if vibed_is_missing(&why) {
            c.upgrade_in_background(&l);
            c.note_still_missing(&l, &why);
        }
        let was_up = l.connected();
        if l.set_down(Some(why.clone())) {
            if was_up {
                crate::log!("vibed: the link to {} is down: {why}", l.name);
            } else {
                crate::log!("vibed: can't link to {}: {why}", l.name);
            }
            c.host_link_changed(&l);
        }
        // A link that held for a while starts over quickly; one that keeps
        // dropping as soon as it's up backs off.
        if was_up && began.elapsed() >= timing.ping_every {
            backoff = timing.min_backoff;
        }
        let wait = backoff;
        backoff = (backoff * 2).min(timing.max_backoff);
        drop(c);
        let kicked = async {
            if let Some(l) = link.upgrade() {
                // Holds the link only while waiting on it; removing the host
                // aborts this task.
                l.kick.notified().await;
            }
        };
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = kicked => backoff = timing.min_backoff,
        }
    }
}

/// One connection's life: hello, the host's report, the event
/// subscription, then events and pings until it ends, or the core does. It
/// holds the core only while it uses it, so a core that's dropped goes.
async fn session(core: &Weak<Core>, link: &Arc<Link>, timing: Timing) -> Result<(), String> {
    let Some(c) = core.upgrade() else {
        return Ok(());
    };
    let started = opening(&c, link, timing).await;
    drop(c);
    let mut channel = started?;
    let mut pings = tokio::time::interval(timing.ping_every);
    pings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    pings.tick().await;
    let mut waiting: Option<(i64, Instant)> = None;
    loop {
        tokio::select! {
            message = channel.read() => {
                let message = message.map_err(|e| e.to_string())?;
                let Some(message) = message else {
                    return Err(channel.why().unwrap_or_else(|| "its vibed closed the link".into()));
                };
                if let Some((id, _)) = waiting
                    && message.get("id") == Some(&json!(id))
                {
                    waiting = None;
                    // The ping's answer is the host's own listing: whether
                    // it serves MCP now, and its external address.
                    let own = message
                        .get("result")
                        .cloned()
                        .and_then(|r| serde_json::from_value::<HostList>(r).ok())
                        .and_then(|l| l.hosts.into_iter().find(|h| h.name == LOCAL_HOST));
                    if let Some(c) = core.upgrade() {
                        if let Some(own) = own
                            && link.set_report(own.clone())
                        {
                            c.host_reported(link, Some(&own), None);
                            c.host_link_changed(link);
                        }
                        // One that failed is tried again, in time.
                        if link.behind.load(Ordering::SeqCst) {
                            c.upgrade_in_background(link);
                        }
                        // Every time, as a report put off for being too soon after
                        // telling it won't change again.
                        c.reconcile_external(link);
                        // A push that failed is tried again.
                        if link.keys_stale.load(Ordering::SeqCst) {
                            c.push_keys_to(link);
                        }
                    }
                    continue;
                }
                if message.get("method").and_then(Value::as_str) == Some("events.event")
                    && let Some(params) = message.get("params")
                {
                    let Some(c) = core.upgrade() else {
                        return Ok(());
                    };
                    c.remote_event(link, params.clone());
                }
            }
            // A ping unanswered for `ping_timeout` ends it, then and there.
            () = overdue(waiting.map(|(_, since)| since + timing.ping_timeout)) => {
                // An install shares this connection, and a big upload can hold a
                // ping up on a slow line. Dropping it would cut that short, to start
                // again from nothing. If the line's really gone, the install says so.
                if lock(&link.upgrade).running {
                    waiting = waiting.map(|(id, _)| (id, Instant::now()));
                    continue;
                }
                // The connection under it is likely gone, as from a pulled
                // network: the next dial starts a new one rather than wait
                // for ssh's keepalives to give up on the shared one.
                if let Some(c) = core.upgrade() {
                    c.remote.drop_shared(&link.address);
                }
                return Err(format!(
                    "it stopped answering (no answer for {} s)",
                    timing.ping_timeout.as_secs()
                ));
            }
            _ = pings.tick() => {
                if core.strong_count() == 0 {
                    return Ok(());
                }
                if waiting.is_some() {
                    continue;
                }
                let id = channel
                    .request(HostsList::NAME, json!({}))
                    .await
                    .map_err(|e| e.to_string())?;
                waiting = Some((id, Instant::now()));
            }
        }
    }
}

/// Resolves at `at`, or never without one.
async fn overdue(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// Dials the host and opens the session: hello, its report, the event
/// subscription. Then the link is up.
async fn opening(core: &Arc<Core>, link: &Arc<Link>, timing: Timing) -> Result<Channel, String> {
    let mut channel =
        tokio::time::timeout(timing.connect_timeout, core.remote.dial_link(&link.address))
            .await
            .map_err(|_| {
                format!(
                    "ssh didn't connect in {} s",
                    timing.connect_timeout.as_secs()
                )
            })??;
    let opening = async {
        let hello = channel
            .call(
                Hello::NAME,
                json!({"client": format!("vibed link from {}", core.label()), "protocol": PROTOCOL_VERSION}),
            )
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = hello {
            if e.code == ErrorCode::ProtocolMismatch.code() {
                let theirs = e
                    .data
                    .as_ref()
                    .and_then(|d| d.get("protocol"))
                    .and_then(Value::as_u64);
                // Only up: this build's vibed speaks this build's protocol,
                // and a newer one's registry is one it can't read.
                if theirs.is_some_and(|p| p < u64::from(PROTOCOL_VERSION)) {
                    core.upgrade_in_background(link);
                    return Err(format!(
                        "its vibed is older, and speaks protocol {}",
                        theirs.unwrap_or_default()
                    ));
                }
                return Err(format!(
                    "its vibed is newer than this Mac's and speaks protocol {}: update VibeOS on \
                     this Mac",
                    theirs.map_or_else(|| "?".into(), |p| p.to_string())
                ));
            }
            return Err(e.message);
        }
        let hosts = channel
            .call(HostsList::NAME, json!({}))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.message)?;
        let subscribed = channel
            .call("events.subscribe", json!({}))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.message)?;
        Ok((hello.ok(), hosts, subscribed))
    };
    let (hello, hosts, _) = tokio::time::timeout(timing.connect_timeout, opening)
        .await
        .map_err(|_| {
            // A shared connection that hangs would hang the next dial too.
            core.remote.drop_shared(&link.address);
            channel.why().unwrap_or_else(|| {
                format!(
                    "its vibed didn't answer in {} s",
                    timing.connect_timeout.as_secs()
                )
            })
        })??;
    let own = serde_json::from_value::<HostList>(hosts)
        .ok()
        .and_then(|l| l.hosts.into_iter().find(|h| h.name == LOCAL_HOST));
    let version = hello
        .as_ref()
        .and_then(|h| h.get("version"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    core.host_reported(link, own.as_ref(), version);
    link.set_up(own);
    crate::log!("vibed: linked to {} ({})", link.name, link.address);
    core.host_link_changed(link);
    core.link_came_up(link);
    Ok(channel)
}

/// Copies an event from a host's stream to this one's, under the host's
/// name. Events about other hosts the host knows are left out.
pub(crate) fn relabel_event(name: &str, mut params: Value) -> Option<Event> {
    if params.get("host").and_then(Value::as_str) != Some(LOCAL_HOST) {
        return None;
    }
    params["host"] = json!(name);
    let mut event: Event = serde_json::from_value(params).ok()?;
    event.seq = 0;
    Some(event)
}

/// Whether what a link's `ssh` said means it reached the server and found no `vibed`
/// to talk to: it's stopped, or isn't there, or won't run. Installing is what puts
/// that right, and changes nothing that's in order. Shells word this differently, but
/// all name the file, which sets it apart from anything of `ssh`'s own.
pub fn vibed_is_missing(why: &str) -> bool {
    let lower = why.to_ascii_lowercase();
    why.contains("can't reach vibed")
        || (why.contains(crate::remote::HOST_VIBE)
            && [
                "not found",
                "no such file",
                "unknown command",
                "permission denied",
                "exec format error",
            ]
            .iter()
            .any(|sign| lower.contains(sign)))
}
