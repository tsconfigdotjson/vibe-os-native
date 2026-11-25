//! MCP Events from the Mac, on a Linux host (PRD: MCP interface, Events): what happens
//! to the Mac's agents, and to those of the other hosts linked to it, heard down the
//! Mac's tunnel, so one endpoint here hears every host it reaches.
//!
//! The tunnel takes nothing without a caller's API key, which the Mac checks itself, and
//! this host keeps only keys' hashes. So it's callers' keys that open the Mac's events
//! (`tunnel.events`): a caller of `events/poll` or `events/stream` here whose key the Mac
//! takes ([`Core::mac_events_heard`]) wants them, and one feed down the tunnel, on a key
//! that's wanted them in the last [`WANTED_FOR`], puts them in the hub, as the Mac says
//! them (its `local` is `mac` here; a host linked to it keeps its name). With no key
//! wanting them, it ends, and no key is kept.
//!
//! They're relayed in the hub ([`vibe_mcp::events::Occurrence::relayed`]), so only callers
//! the Mac takes hear them: a key of this host's own hears nothing of the Mac's, as its
//! calls get nothing there. Each time the feed subscribes, whatever came before it might
//! have been missed (the Mac slept, the tunnel went, nothing wanted them for a while), so
//! that's a gap in what's relayed: `truncated` for every cursor from before it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::watch;
use vibe_core::model::{LOCAL_HOST, MAC_HOST};
use vibe_core::rpc::{ErrorCode, Event, HostsList, Method, TUNNEL_EVENTS, TunnelEvents};
use vibe_mcp::events::Hub;

use crate::core::{Core, lock};
use crate::remote::Channel;

/// How long what the Mac said of a key holds, unless the keys change first.
const VERDICT_FOR: Duration = Duration::from_secs(60);

/// How long a key that asked for events keeps the feed going.
pub const WANTED_FOR: Duration = Duration::from_secs(10 * 60);

/// How long a request waits for a feed it started to subscribe, so a first poll's cursor
/// is after the gap that subscribing is.
const FIRST_WAIT: Duration = Duration::from_secs(3);

/// How long the Mac gets to answer a subscription.
const SUBSCRIBE_WITHIN: Duration = Duration::from_secs(10);

/// How often the feed looks at whether to go on.
const LOOK_EVERY: Duration = Duration::from_millis(250);

/// The waits between tries at subscribing.
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// What the Mac said of a key.
#[derive(Debug, Clone, Copy)]
struct Verdict {
    takes: bool,
    at: Instant,
    /// The hub's key epoch then: a change of keys here (as when the Mac pushes a revoke)
    /// asks again.
    epoch: u64,
}

#[derive(Debug, Default)]
struct Keys {
    /// By key hash.
    verdicts: HashMap<String, Verdict>,
    /// Keys that asked for events, by hash: the key, and when it last asked.
    wanting: HashMap<String, (String, Instant)>,
}

/// The Mac's events, heard here.
#[derive(Debug)]
pub(crate) struct Relay {
    keys: Mutex<Keys>,
    /// Whether the feed's task is running.
    running: AtomicBool,
    /// Whether a subscription is open now.
    live: watch::Sender<bool>,
}

impl Default for Relay {
    fn default() -> Self {
        Relay {
            keys: Mutex::new(Keys::default()),
            running: AtomicBool::new(false),
            live: watch::Sender::new(false),
        }
    }
}

impl Relay {
    fn verdict(&self, hash: &str, epoch: u64) -> Option<bool> {
        lock(&self.keys)
            .verdicts
            .get(hash)
            .filter(|v| v.epoch == epoch && v.at.elapsed() < VERDICT_FOR)
            .map(|v| v.takes)
    }

    fn judge(&self, hash: &str, takes: bool, epoch: u64) {
        let mut keys = lock(&self.keys);
        keys.verdicts.insert(
            hash.to_owned(),
            Verdict {
                takes,
                at: Instant::now(),
                epoch,
            },
        );
        if !takes {
            keys.wanting.remove(hash);
        }
        keys.verdicts
            .retain(|_, v| v.at.elapsed() < VERDICT_FOR * 10);
    }

    fn want(&self, hash: &str, key: &str) {
        lock(&self.keys)
            .wanting
            .insert(hash.to_owned(), (key.to_owned(), Instant::now()));
    }

    /// The key for the feed: `current` while it's still wanted, or else the one that
    /// asked last; none once none has for [`WANTED_FOR`].
    fn pick(&self, current: Option<&str>) -> Option<(String, String)> {
        let mut keys = lock(&self.keys);
        keys.wanting.retain(|_, (_, at)| at.elapsed() < WANTED_FOR);
        if let Some(h) = current
            && let Some((key, _)) = keys.wanting.get(h)
        {
            return Some((h.to_owned(), key.clone()));
        }
        keys.wanting
            .iter()
            .max_by_key(|(_, (_, at))| *at)
            .map(|(h, (key, _))| (h.clone(), key.clone()))
    }

    fn still_wanted(&self, hash: &str) -> bool {
        lock(&self.keys)
            .wanting
            .get(hash)
            .is_some_and(|(_, at)| at.elapsed() < WANTED_FOR)
    }
}

/// How the Mac answered a subscription.
enum Subscribed {
    Open(Channel, TunnelEvents),
    /// It doesn't take the key.
    Refused,
    Failed(String),
}

/// What the Mac's host is called here: its own is `mac`, and a host linked to it keeps
/// its name.
fn as_here(host: &str) -> String {
    if host == LOCAL_HOST {
        MAC_HOST.to_owned()
    } else {
        host.to_owned()
    }
}

async fn subscribe(socket: PathBuf, key: &str) -> Subscribed {
    let stream = match UnixStream::connect(&socket).await {
        Ok(s) => s,
        Err(e) => return Subscribed::Failed(format!("its tunnel is down: {e}")),
    };
    let (read, write) = stream.into_split();
    let mut channel = Channel::new(
        BufReader::new(read),
        write,
        (),
        Arc::new(Mutex::new(String::new())),
    );
    let call = channel.call(TUNNEL_EVENTS, json!({ "key": key }));
    match tokio::time::timeout(SUBSCRIBE_WITHIN, call).await {
        Err(_) => Subscribed::Failed("it didn't answer".into()),
        Ok(Err(e)) => Subscribed::Failed(e.to_string()),
        Ok(Ok(Err(e))) if e.code == ErrorCode::Unauthorized.code() => Subscribed::Refused,
        Ok(Ok(Err(e))) => Subscribed::Failed(e.message),
        Ok(Ok(Ok(v))) => match serde_json::from_value(v) {
            Ok(states) => Subscribed::Open(channel, states),
            Err(e) => Subscribed::Failed(format!("it answered with something unexpected: {e}")),
        },
    }
}

/// Puts what comes down an open subscription in the hub, until it ends, the Mac's found
/// gone, or the key it's on is no longer wanted. Says why it ended.
async fn hear(core: &Weak<Core>, hub: &Hub, channel: &mut Channel, hash: &str) -> String {
    let mut look = tokio::time::interval(LOOK_EVERY);
    loop {
        tokio::select! {
            message = channel.read() => match message {
                Ok(Some(m)) => {
                    if m.get("method").and_then(Value::as_str) != Some("events.event") {
                        continue;
                    }
                    let Some(mut e) = m
                        .get("params")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<Event>(p).ok())
                    else {
                        continue;
                    };
                    e.host = as_here(&e.host);
                    hub.ingest_relayed(&e);
                }
                Ok(None) => return "the Mac closed it".into(),
                Err(e) => return format!("it failed: {e}"),
            },
            _ = look.tick() => {
                let Some(c) = core.upgrade() else {
                    return "this vibed is stopping".into();
                };
                if c.mac().is_none() || c.peer.is_gone() {
                    return "the Mac stopped answering".into();
                }
                if !c.relay.still_wanted(hash) {
                    return "no caller wants it now".into();
                }
            }
        }
    }
}

/// The feed: subscribes down the tunnel on a wanted key, and again whenever that ends,
/// until no key wants it.
async fn feed(core: Weak<Core>, hub: Arc<Hub>) {
    let mut current: Option<String> = None;
    let mut backoff = MIN_BACKOFF;
    loop {
        let (hash, key, socket) = {
            let Some(c) = core.upgrade() else { return };
            let Some((hash, key)) = c.relay.pick(current.as_deref()) else {
                c.relay.running.store(false, Ordering::SeqCst);
                // A key that wanted it just then has found it running: go on for it.
                if c.relay.pick(None).is_some()
                    && c.relay
                        .running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    continue;
                }
                return;
            };
            let ready = c.mac().is_some_and(|m| m.is_up() && m.serves_events());
            if !ready {
                drop(c);
                tokio::time::sleep(LOOK_EVERY).await;
                continue;
            }
            (hash, key, c.peer.socket().to_owned())
        };
        current = Some(hash.clone());
        match subscribe(socket, &key).await {
            Subscribed::Open(mut channel, states) => {
                backoff = MIN_BACKOFF;
                for a in &states.agents {
                    hub.seed_relayed(&as_here(&a.host), &a.agent, &a.state);
                }
                for l in &states.links {
                    hub.seed_link(&as_here(&l.host), &l.link);
                }
                // Whatever came before this was never heard here.
                hub.relayed_gap();
                let Some(c) = core.upgrade() else { return };
                c.relay.live.send_replace(true);
                crate::log!("vibed: hearing the Mac's events through its tunnel");
                drop(c);
                let why = hear(&core, &hub, &mut channel, &hash).await;
                let Some(c) = core.upgrade() else { return };
                c.relay.live.send_replace(false);
                crate::log!("vibed: stopped hearing the Mac's events: {why}");
            }
            Subscribed::Refused => {
                let Some(c) = core.upgrade() else { return };
                c.relay.judge(&hash, false, hub.key_epoch());
                current = None;
            }
            Subscribed::Failed(why) => {
                crate::log!("vibed: can't hear the Mac's events: {why}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

impl Core {
    /// Whether the caller with `key` hears the Mac's events here, as the Mac says: asked of
    /// it now and then, with what it said kept for [`VERDICT_FOR`], or until the keys
    /// change. One it takes keeps the feed of them going, and waits a moment for a feed it
    /// starts. On a host with no Mac, or one too old to say, there's nothing to hear.
    pub(crate) async fn mac_events_heard(&self, key: &str, hub: &Arc<Hub>) -> bool {
        let Some(mac) = self.mac() else { return false };
        if !mac.serves_events() {
            return false;
        }
        let hash = vibe_core::keys::hash(key);
        let epoch = hub.key_epoch();
        let takes = match self.relay.verdict(&hash, epoch) {
            Some(takes) => takes,
            None => match mac
                .call(key, HostsList::NAME, json!({}), SUBSCRIBE_WITHIN)
                .await
            {
                Ok(_) => {
                    self.relay.judge(&hash, true, epoch);
                    true
                }
                Err(e) if e.code == ErrorCode::Unauthorized.code() => {
                    self.relay.judge(&hash, false, epoch);
                    false
                }
                // It can't say now: what it last said, for these keys.
                Err(_) => lock(&self.relay.keys)
                    .verdicts
                    .get(&hash)
                    .is_some_and(|v| v.takes && v.epoch == epoch),
            },
        };
        if !takes {
            return false;
        }
        self.relay.want(&hash, key);
        let starting = self
            .relay
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok();
        if starting {
            match (self.me.get(), self.runtime.get()) {
                (Some(me), Some(rt)) => {
                    rt.spawn(feed(me.clone(), Arc::clone(hub)));
                }
                _ => {
                    self.relay.running.store(false, Ordering::SeqCst);
                    return true;
                }
            }
            // Only for a feed it started: one subscribing again after the Mac slept is a
            // gap that's really there for whatever cursor this request has.
            if mac.is_up() {
                let mut live = self.relay.live.subscribe();
                let _ = tokio::time::timeout(FIRST_WAIT, live.wait_for(|l| *l)).await;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feed_keeps_its_key_while_wanted_and_lets_go_of_one_refused() {
        let relay = Relay::default();
        assert!(relay.pick(None).is_none());
        relay.want("h1", "vk_one");
        relay.want("h2", "vk_two");
        assert_eq!(relay.pick(Some("h1")).unwrap().1, "vk_one");
        assert_eq!(relay.pick(None).unwrap().1, "vk_two", "the last to ask");
        relay.judge("h2", false, 0);
        assert!(!relay.still_wanted("h2"));
        assert_eq!(relay.pick(Some("h2")).unwrap().1, "vk_one");
        assert_eq!(relay.verdict("h2", 0), Some(false));
        assert_eq!(relay.verdict("h2", 1), None, "the keys changed since");
    }

    #[test]
    fn the_macs_own_goes_by_mac_and_its_hosts_keep_their_names() {
        assert_eq!(as_here("local"), "mac");
        assert_eq!(as_here("edge"), "edge");
    }
}
