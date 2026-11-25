//! MCP Events from the Mac, on a Linux host (PRD: MCP interface, Events): what happens
//! to the Mac's agents, and to those of the other hosts linked to it, heard down the
//! Mac's tunnel, so one endpoint here hears every host it reaches, for webhook
//! subscriptions that run with no caller around.
//!
//! The tunnel takes nothing without a caller's API key, which the Mac checks itself, and
//! this host keeps only keys' hashes. So when a key subscribes here, its key is traded
//! with the Mac for a relay grant (`tunnel.grant`; [`crate::relay_grant`] on the Mac),
//! which is kept (`mac-relay.json`, its owner's alone) in its place: a grant opens the
//! Mac's events and nothing else, and ends with the key. One feed down the tunnel
//! (`tunnel.events`), on the grants of every key with a subscription here, puts the Mac's
//! events in the hub as the Mac says them (its `local` is `mac` here; a host linked to it
//! keeps its name), and the Mac says which grants it still takes as keys change.
//!
//! They're relayed in the hub ([`vibe_mcp::events::Occurrence::relayed`]), so only
//! subscriptions whose key the Mac takes hear them: a key of this host's own gets no
//! grant, and hears nothing of the Mac's, as its calls get nothing there. Each time the
//! feed subscribes, whatever came before it might have been missed (the Mac slept, the
//! tunnel went), so that's a gap in what's relayed, `truncated` at the next refresh.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::watch;
use vibe_core::model::{LOCAL_HOST, MAC_HOST};
use vibe_core::rpc::{
    ErrorCode, Event, TUNNEL_EVENTS, TUNNEL_GRANT, TunnelAdmitted, TunnelEvents, TunnelGrant,
};
use vibe_mcp::events::Hub;

use crate::core::{Core, lock};
use crate::remote::Channel;

/// Where grants are kept, beside `vibe.db`.
const GRANTS_FILE: &str = "mac-relay.json";

/// A grant with less than this left is taken again at its key's next subscribe or
/// refresh: more than the longest subscription granted (30 days).
const RENEW_WITHIN: i64 = 31 * 24 * 3600;

/// How long a subscribe waits for the feed to take its key, so its cursor is after the
/// gap that subscribing is.
const FIRST_WAIT: Duration = Duration::from_secs(3);

/// How long a key that's subscribing counts as having a subscription, before it's said.
const SUBSCRIBING_FOR: Duration = Duration::from_secs(30);

/// How long the Mac gets to answer.
const ANSWER_WITHIN: Duration = Duration::from_secs(10);

/// How often the feed looks at whether to go on as it is.
const LOOK_EVERY: Duration = Duration::from_millis(250);

/// The waits between tries at subscribing.
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// How long the feed waits after the Mac took none of its grants, unless they change.
const REFUSED_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Held {
    grant: String,
    expires: i64,
}

impl fmt::Debug for Held {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Held")
            .field("expires", &self.expires)
            .finish_non_exhaustive()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Kept {
    /// By key hash.
    grants: BTreeMap<String, Held>,
}

#[derive(Debug, Default)]
struct Keys {
    loaded: bool,
    /// By key hash.
    grants: BTreeMap<String, Held>,
    /// Keys with webhook subscriptions here, by hash.
    owners: BTreeSet<String>,
    /// Keys subscribing now, by hash, and since when.
    subscribing: HashMap<String, Instant>,
}

/// The Mac's events, heard here.
#[derive(Debug)]
pub(crate) struct Relay {
    keys: Mutex<Keys>,
    /// Keys the Mac takes, on the feed that's open, by hash.
    admitted: Mutex<HashSet<String>>,
    /// Whether the feed's task is running.
    running: AtomicBool,
    /// Bumped whenever `admitted` changes.
    heard: watch::Sender<u64>,
}

impl Default for Relay {
    fn default() -> Self {
        Relay {
            keys: Mutex::new(Keys::default()),
            admitted: Mutex::new(HashSet::new()),
            running: AtomicBool::new(false),
            heard: watch::Sender::new(0),
        }
    }
}

impl Relay {
    /// Whether the key with this hash hears the Mac's events now.
    pub(crate) fn hears(&self, owner: &str) -> bool {
        lock(&self.admitted).contains(owner)
    }

    fn admit(&self, owners: HashSet<String>) {
        *lock(&self.admitted) = owners;
        self.heard.send_modify(|n| *n += 1);
    }

    /// The grants the feed should be on now: of keys with subscriptions, or subscribing,
    /// that haven't run out. In order, so a change shows.
    fn wanted(&self, now: i64) -> Vec<(String, String)> {
        let mut keys = lock(&self.keys);
        keys.subscribing
            .retain(|_, since| since.elapsed() < SUBSCRIBING_FOR);
        let keys = &*keys;
        keys.grants
            .iter()
            .filter(|(owner, held)| {
                held.expires > now
                    && (keys.owners.contains(*owner) || keys.subscribing.contains_key(*owner))
            })
            .map(|(owner, held)| (owner.clone(), held.grant.clone()))
            .collect()
    }
}

/// How the Mac answered.
enum Asked<T> {
    Gave(T),
    /// It doesn't take the key, or any of the grants.
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

/// Calls `method` down the tunnel at `socket`, on a connection of its own.
async fn ask(socket: &Path, method: &str, params: Value) -> Asked<(Channel, Value)> {
    let stream = match UnixStream::connect(socket).await {
        Ok(s) => s,
        Err(e) => return Asked::Failed(format!("its tunnel is down: {e}")),
    };
    let (read, write) = stream.into_split();
    let mut channel = Channel::new(
        BufReader::new(read),
        write,
        (),
        Arc::new(Mutex::new(String::new())),
    );
    let call = channel.call(method, params);
    match tokio::time::timeout(ANSWER_WITHIN, call).await {
        Err(_) => Asked::Failed("it didn't answer".into()),
        Ok(Err(e)) => Asked::Failed(e.to_string()),
        Ok(Ok(Err(e))) if e.code == ErrorCode::Unauthorized.code() => Asked::Refused,
        Ok(Ok(Err(e))) => Asked::Failed(e.message),
        Ok(Ok(Ok(v))) => Asked::Gave((channel, v)),
    }
}

/// Puts what comes down an open feed in the hub, until it ends, the Mac's found gone, or
/// the grants it should be on change. Says why it ended.
async fn hear(
    core: &Weak<Core>,
    hub: &Hub,
    channel: &mut Channel,
    on: &[(String, String)],
) -> String {
    let mut look = tokio::time::interval(LOOK_EVERY);
    loop {
        tokio::select! {
            message = channel.read() => match message {
                Ok(Some(m)) => match m.get("method").and_then(Value::as_str) {
                    Some("events.event") => {
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
                    Some("events.admitted") => {
                        let Some(a) = m
                            .get("params")
                            .cloned()
                            .and_then(|p| serde_json::from_value::<TunnelAdmitted>(p).ok())
                        else {
                            continue;
                        };
                        let Some(c) = core.upgrade() else {
                            return "this vibed is stopping".into();
                        };
                        c.relay.admit(admitted(on, &a.admitted));
                    }
                    _ => {}
                },
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
                if c.relay.wanted(c.now()) != on {
                    return "the keys with subscriptions changed".into();
                }
            }
        }
    }
}

/// The keys of `on` the Mac said it takes.
fn admitted(on: &[(String, String)], said: &[bool]) -> HashSet<String> {
    on.iter()
        .zip(said)
        .filter(|(_, yes)| **yes)
        .map(|((owner, _), _)| owner.clone())
        .collect()
}

/// The feed: subscribes down the tunnel on the grants wanted, and again whenever that
/// ends or they change, until none are.
async fn feed(core: Weak<Core>) {
    let mut backoff = MIN_BACKOFF;
    loop {
        let (on, socket, hub) = {
            let Some(c) = core.upgrade() else { return };
            let on = c.relay.wanted(c.now());
            if on.is_empty() {
                c.relay.running.store(false, Ordering::SeqCst);
                // Wanted just then, and found it running: go on for it.
                if !c.relay.wanted(c.now()).is_empty()
                    && c.relay
                        .running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok()
                {
                    continue;
                }
                return;
            }
            let ready = c.mac().is_some_and(|m| m.is_up() && m.serves_events());
            let hub = c.mcp_events.get().map(|w| Arc::clone(w.hub()));
            let (true, Some(hub)) = (ready, hub) else {
                drop(c);
                tokio::time::sleep(LOOK_EVERY).await;
                continue;
            };
            (on, c.peer.socket().to_owned(), hub)
        };
        let grants: Vec<&String> = on.iter().map(|(_, g)| g).collect();
        match ask(&socket, TUNNEL_EVENTS, json!({ "grants": grants })).await {
            Asked::Gave((mut channel, answer)) => {
                let Ok(states) = serde_json::from_value::<TunnelEvents>(answer) else {
                    crate::log!(
                        "vibed: can't hear the Mac's events: it answered with something unexpected"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                };
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
                let heard = admitted(&on, &states.admitted);
                crate::log!(
                    "vibed: hearing the Mac's events through its tunnel, for {} key(s)",
                    heard.len()
                );
                c.relay.admit(heard);
                drop(c);
                let why = hear(&core, &hub, &mut channel, &on).await;
                let Some(c) = core.upgrade() else { return };
                c.relay.admit(HashSet::new());
                crate::log!("vibed: stopped hearing the Mac's events: {why}");
            }
            Asked::Refused => {
                crate::log!("vibed: the Mac takes none of the relay grants kept here");
                let Some(c) = core.upgrade() else { return };
                c.relay.admit(HashSet::new());
                drop(c);
                // Until a key subscribes again, with a grant taken again.
                let until = Instant::now() + REFUSED_WAIT;
                while Instant::now() < until {
                    tokio::time::sleep(LOOK_EVERY).await;
                    let Some(c) = core.upgrade() else { return };
                    if c.relay.wanted(c.now()) != on {
                        break;
                    }
                }
            }
            Asked::Failed(why) => {
                crate::log!("vibed: can't hear the Mac's events: {why}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(MAX_BACKOFF);
            }
        }
    }
}

impl Core {
    fn grants_path(&self) -> PathBuf {
        self.layout.data_dir().join(GRANTS_FILE)
    }

    /// The grants kept, read the first time.
    fn load_grants(&self, keys: &mut Keys) {
        if keys.loaded {
            return;
        }
        keys.loaded = true;
        if let Ok(bytes) = std::fs::read(self.grants_path())
            && let Ok(kept) = serde_json::from_slice::<Kept>(&bytes)
        {
            keys.grants = kept.grants;
        }
    }

    fn save_grants(&self, keys: &Keys) {
        let kept = Kept {
            grants: keys.grants.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&kept).expect("grants serialize");
        if let Err(e) = vibe_mcp::webhook::write_private(&self.grants_path(), &bytes) {
            crate::log!("vibed: can't keep the Mac's relay grants: {e}");
        }
    }

    /// Starts the feed, unless it's running.
    fn kick_feed(&self) {
        if self
            .relay
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        match (self.me.get(), self.runtime.get()) {
            (Some(me), Some(rt)) => {
                rt.spawn(feed(me.clone()));
            }
            _ => self.relay.running.store(false, Ordering::SeqCst),
        }
    }

    /// The keys with webhook subscriptions here changed: the feed follows them, and the
    /// grants of keys with none any more go.
    pub(crate) fn relay_owners_changed(&self, owners: &[String]) {
        let mut keys = lock(&self.relay.keys);
        self.load_grants(&mut keys);
        keys.owners = owners.iter().cloned().collect();
        let before = keys.grants.len();
        let Keys {
            grants,
            owners,
            subscribing,
            ..
        } = &mut *keys;
        grants.retain(|owner, _| owners.contains(owner) || subscribing.contains_key(owner));
        if keys.grants.len() != before {
            self.save_grants(&keys);
        }
        let any = !keys.grants.is_empty();
        drop(keys);
        if any {
            self.kick_feed();
        }
    }

    /// A key is subscribing (`events/subscribe`): on a host with a Mac that serves events,
    /// get the Mac's grant for it, unless one's kept that has long enough left, and wait a
    /// moment for the feed to take it. A key the Mac doesn't take gets none, and hears
    /// nothing of the Mac's.
    pub(crate) async fn mac_relay_for(&self, key: &str) {
        let Some(mac) = self.mac() else { return };
        if !mac.serves_events() {
            return;
        }
        let owner = vibe_core::keys::hash(key);
        let fresh = {
            let mut keys = lock(&self.relay.keys);
            self.load_grants(&mut keys);
            keys.subscribing.insert(owner.clone(), Instant::now());
            keys.grants
                .get(&owner)
                .is_some_and(|h| h.expires - self.now() > RENEW_WITHIN)
        };
        if !fresh && mac.is_up() {
            match ask(self.peer.socket(), TUNNEL_GRANT, json!({ "key": key })).await {
                Asked::Gave((_, answer)) => {
                    if let Ok(g) = serde_json::from_value::<TunnelGrant>(answer) {
                        let mut keys = lock(&self.relay.keys);
                        keys.grants.insert(
                            owner.clone(),
                            Held {
                                grant: g.grant,
                                expires: g.expires,
                            },
                        );
                        self.save_grants(&keys);
                    }
                }
                Asked::Refused => {
                    let mut keys = lock(&self.relay.keys);
                    if keys.grants.remove(&owner).is_some() {
                        self.save_grants(&keys);
                    }
                    return;
                }
                Asked::Failed(why) => {
                    crate::log!("vibed: can't get the Mac's relay grant for a key: {why}");
                }
            }
        }
        let has = lock(&self.relay.keys).grants.contains_key(&owner);
        if !has {
            return;
        }
        self.kick_feed();
        if mac.is_up() && !self.relay.hears(&owner) {
            let mut heard = self.relay.heard.subscribe();
            let wait = async {
                while !self.relay.hears(&owner) {
                    if heard.changed().await.is_err() {
                        return;
                    }
                }
            };
            let _ = tokio::time::timeout(FIRST_WAIT, wait).await;
        }
    }
}

/// What webhook subscriptions here ask of `vibed`: whether their keys are still keys,
/// and whether they hear the Mac's events.
#[derive(Debug)]
pub(crate) struct Owners(pub(crate) Weak<Core>);

impl vibe_mcp::webhook::Owners for Owners {
    fn holds(&self, owner: &str) -> bool {
        let Some(c) = self.0.upgrade() else {
            return true;
        };
        // Only a key that's surely gone ends what it subscribed.
        !matches!(c.store().key_by_hash(owner), Ok(None))
    }

    fn hears_relayed(&self, owner: &str) -> bool {
        self.0.upgrade().is_some_and(|c| c.relay.hears(owner))
    }

    fn owners_changed(&self, owners: &[String]) {
        if let Some(c) = self.0.upgrade()
            && c.mac().is_some()
        {
            c.relay_owners_changed(owners);
        }
    }

    fn log(&self, line: &str) {
        crate::log!("vibed: {line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feed_is_on_the_grants_of_keys_with_subscriptions() {
        let relay = Relay::default();
        let held = |g: &str, expires| Held {
            grant: g.into(),
            expires,
        };
        {
            let mut keys = lock(&relay.keys);
            keys.grants.insert("h1".into(), held("g1", 100));
            keys.grants.insert("h2".into(), held("g2", 100));
            keys.grants.insert("h3".into(), held("g3", 10));
            keys.owners = ["h1", "h3"].iter().map(|s| s.to_string()).collect();
        }
        assert_eq!(relay.wanted(50), [("h1".to_owned(), "g1".to_owned())]);
        lock(&relay.keys)
            .subscribing
            .insert("h2".into(), Instant::now());
        assert_eq!(relay.wanted(50).len(), 2, "one subscribing counts");
        assert!(relay.wanted(200).is_empty(), "they ran out");
        let on = relay.wanted(50);
        assert_eq!(
            admitted(&on, &[false, true]),
            HashSet::from(["h2".to_owned()])
        );
        relay.admit(admitted(&on, &[true, true]));
        assert!(relay.hears("h1") && relay.hears("h2") && !relay.hears("h3"));
    }

    #[test]
    fn the_macs_own_goes_by_mac_and_its_hosts_keep_their_names() {
        assert_eq!(as_here("local"), "mac");
        assert_eq!(as_here("edge"), "edge");
    }

    #[test]
    fn a_grants_debug_shows_no_grant() {
        let h = Held {
            grant: "vgr1.secret".into(),
            expires: 5,
        };
        assert!(!format!("{h:?}").contains("secret"));
    }
}
