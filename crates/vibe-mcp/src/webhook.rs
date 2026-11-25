//! Webhook delivery of MCP Events (PRD: MCP interface, Events), the one mode
//! that works between a client's turns: ChatGPT subscribes with a callback
//! URL and a signing secret (`events/subscribe`), and every event that
//! matches is posted there, signed as Standard Webhooks says, whether or not
//! anything of the client is running.
//!
//! A subscription is its owner's (the key it was made with, by hash: never
//! the key), its callback URL, its event type and its arguments, and its id
//! is derived from those, so subscribing again is a refresh. Before the
//! first event to a URL, the URL is verified: a signed challenge it must
//! echo. Each event is posted alone, retried with backoff on failure (never
//! on 410 or 413), and given up after the last try.
//!
//! Subscriptions live in memory only, for the lifetime granted
//! ([`Config::max_ttl`] at most), and a restart ends them all. The client's
//! next refresh makes its subscription again: the same id, its URL verified
//! again, and `truncated: true`, since its cursor is from a process that's
//! gone. A key that's revoked ends its subscriptions; one that expires ends
//! without being refreshed.
//!
//! Every subscription reads the [`Hub`]: its cursor is a position there, and
//! a gap since the cursor a client refreshes with is `truncated: true`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use rmcp::ErrorData;
use rmcp::model::ErrorCode;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::{Notify, Semaphore, watch};
use vibe_core::keys::{hex, hmac_sha256, same_secret};

use crate::Caller;
use crate::egress::{self, Failure, Policy};
use crate::events::{self, Filter, Hub, Name, Occurrence};

/// `CallbackEndpointError`, as ChatGPT has it (SEP-3415 has -32027; this is
/// in the range it leaves to servers, so it can't mean anything else).
pub const CALLBACK_ENDPOINT_ERROR: i32 = -32015;

/// The largest body posted.
pub const MAX_BODY: usize = 256 * 1024;

/// The most events waiting to be delivered to one subscription; past it,
/// new ones are given up.
const MAX_PENDING: usize = 1024;

/// How many deliveries are under way at once, across every subscription.
const SENDING_AT_ONCE: usize = 16;

/// What a subscription's owner may do, as the daemon says.
pub trait Owners: Send + Sync + 'static {
    /// Whether the key with this hash is still one.
    fn holds(&self, owner: &str) -> bool;
    /// Whether the owner hears what another daemon relays
    /// ([`Occurrence::relayed`]).
    fn hears_relayed(&self, _owner: &str) -> bool {
        false
    }
    /// The owners with subscriptions now, whenever that changes.
    fn owners_changed(&self, _owners: &[String]) {}
    fn log(&self, line: &str);
}

/// How subscriptions are granted and delivered.
#[derive(Debug, Clone)]
pub struct Config {
    pub policy: Policy,
    /// The waits between tries at a delivery: one more try than waits.
    pub retries: Vec<Duration>,
    /// How long one post may take.
    pub timeout: Duration,
    pub default_ttl: Duration,
    pub min_ttl: Duration,
    /// The longest lifetime granted, also for `ttlMs: null`: never none.
    pub max_ttl: Duration,
    /// How long a verified URL stays verified for its owner's next
    /// subscriptions to it.
    pub verified_for: Duration,
    /// How long a replaced secret still signs.
    pub rotation: Duration,
    pub max_per_owner: usize,
    pub max_total: usize,
    /// How often expired subscriptions are dropped.
    pub sweep_every: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            policy: Policy::default(),
            // Five tries over about ten minutes.
            retries: vec![
                Duration::from_secs(5),
                Duration::from_secs(30),
                Duration::from_secs(120),
                Duration::from_secs(480),
            ],
            timeout: Duration::from_secs(10),
            // Short, as subscriptions live in memory: after a restart, a client
            // hears nothing until it refreshes.
            default_ttl: Duration::from_secs(3600),
            min_ttl: Duration::from_secs(5 * 60),
            max_ttl: Duration::from_secs(24 * 3600),
            verified_for: Duration::from_secs(24 * 3600),
            rotation: Duration::from_secs(5 * 60),
            max_per_owner: 32,
            max_total: 256,
            sweep_every: Duration::from_secs(30),
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn iso(ms: u64) -> String {
    vibe_core::time::rfc3339((ms / 1000) as i64)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn invalid(message: impl Into<String>) -> ErrorData {
    ErrorData::invalid_params(message.into(), None)
}

/// `value` as canonical JSON: object keys sorted, no spaces. So arguments
/// that differ only in their keys' order are the same.
pub fn canonical(value: &Value) -> String {
    fn write(v: &Value, out: &mut String) {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                out.push('{');
                for (i, k) in keys.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::from(k.as_str()).to_string());
                    out.push(':');
                    write(&m[k], out);
                }
                out.push('}');
            }
            Value::Array(a) => {
                out.push('[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(x, out);
                }
                out.push(']');
            }
            other => out.push_str(&other.to_string()),
        }
    }
    let mut out = String::new();
    write(value, &mut out);
    out
}

/// A subscription's id: over its owner, URL, event type and arguments.
pub fn subscription_id(owner: &str, url: &str, name: &str, arguments: &Value) -> String {
    let mut h = Sha256::new();
    for part in [owner, url, name, &canonical(arguments)] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    format!("sub_{}", &hex(&h.finalize())[..32])
}

/// The signing key in a `whsec_` secret, if it is one: Base64 of 24 to 64
/// bytes.
pub fn secret_key(secret: &str) -> Option<Vec<u8>> {
    let key = STANDARD.decode(secret.strip_prefix("whsec_")?).ok()?;
    (24..=64).contains(&key.len()).then_some(key)
}

/// `webhook-signature` for a message, as Standard Webhooks signs it: one
/// `v1,` signature per secret, space-separated.
pub fn sign(secrets: &[String], id: &str, timestamp: u64, body: &[u8]) -> String {
    let ts = timestamp.to_string();
    secrets
        .iter()
        .filter_map(|s| secret_key(s))
        .map(|key| {
            let mac = hmac_sha256(&key, &[id.as_bytes(), b".", ts.as_bytes(), b".", body]);
            format!("v1,{}", STANDARD.encode(mac))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// What a subscription is, as it was asked for.
struct Terms {
    id: String,
    /// Its key's hash.
    owner: String,
    /// Its key's name, for the log.
    owner_name: String,
    url: String,
    secret: String,
    /// The secret it replaced, and until when that still signs.
    old_secret: Option<String>,
    old_until_ms: u64,
    expires_ms: u64,
}

/// A subscription.
struct Sub {
    terms: Terms,
    name: Name,
    filter: Filter,
    /// The hub's position it's been read to.
    scanned: u64,
    /// Events read and not yet delivered or given up, by position.
    pending: BTreeSet<u64>,
    last_delivery_ms: Option<u64>,
    last_error: Option<&'static str>,
}

impl Sub {
    /// The position that everything up to has been delivered or given up.
    fn watermark(&self) -> u64 {
        self.pending.first().map_or(self.scanned, |p| p - 1)
    }

    /// The cursor event `seq`'s delivery carries: as [`Sub::watermark`], but for it,
    /// as it's delivered if the receiver has it at all.
    fn watermark_with(&self, seq: u64) -> u64 {
        self.pending
            .iter()
            .find(|p| **p != seq)
            .map_or(self.scanned, |p| p - 1)
    }

    fn secrets(&self, now: u64) -> Vec<String> {
        let mut out = vec![self.terms.secret.clone()];
        if let Some(old) = &self.terms.old_secret
            && self.terms.old_until_ms > now
        {
            out.push(old.clone());
        }
        out
    }
}

#[derive(Default)]
struct State {
    subs: HashMap<String, Sub>,
    /// By owner and URL: until when.
    verified: HashMap<(String, String), u64>,
    /// The owners last said to [`Owners::owners_changed`].
    owners: Option<Vec<String>>,
}

impl State {
    fn owners(&self) -> Vec<String> {
        let set: BTreeSet<&String> = self.subs.values().map(|s| &s.terms.owner).collect();
        set.into_iter().cloned().collect()
    }
}

/// An `events/subscribe`, checked.
pub struct Subscribe {
    owner: String,
    owner_name: String,
    name: Name,
    filter: Filter,
    url: String,
    secret: String,
    cursor: Option<String>,
    ttl: Duration,
    id: String,
}

impl fmt::Debug for Subscribe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Subscribe")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Every webhook subscription, and their delivery.
pub struct Webhooks {
    hub: Arc<Hub>,
    config: Config,
    state: Mutex<State>,
    wake: Notify,
    owners: OnceLock<Arc<dyn Owners>>,
    sending: Semaphore,
    /// Set once delivery stops.
    stop: watch::Sender<bool>,
}

impl fmt::Debug for Webhooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Webhooks")
            .field("subscriptions", &lock(&self.state).subs.len())
            .finish_non_exhaustive()
    }
}

impl Webhooks {
    /// No subscriptions yet, read from `hub` once there are. Nothing's
    /// delivered before [`Webhooks::start`].
    pub fn new(hub: Arc<Hub>, config: Config) -> Arc<Webhooks> {
        Arc::new(Webhooks {
            hub,
            config,
            state: Mutex::new(State::default()),
            wake: Notify::new(),
            owners: OnceLock::new(),
            sending: Semaphore::new(SENDING_AT_ONCE),
            stop: watch::Sender::new(false),
        })
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Starts delivering, asking `owners` what their keys may do.
    pub fn start(self: &Arc<Self>, owners: Arc<dyn Owners>) {
        if self.owners.set(owners).is_err() {
            return;
        }
        tokio::spawn(Arc::clone(self).run());
    }

    /// Stops delivering, as when the daemon stops: nothing more is posted.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves once delivery stops.
    async fn stopped(&self) {
        let _ = self.stop.subscribe().wait_for(|s| *s).await;
    }

    fn log(&self, line: &str) {
        match self.owners.get() {
            Some(o) => o.log(line),
            None => eprintln!("{line}"),
        }
    }

    /// How many subscriptions there are.
    pub fn len(&self) -> usize {
        lock(&self.state).subs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The owners with subscriptions, by key hash.
    pub fn owners(&self) -> Vec<String> {
        lock(&self.state).owners()
    }

    /// Checks an `events/subscribe` from `caller`.
    pub fn check_subscribe(
        &self,
        caller: &Caller,
        params: Option<&Value>,
    ) -> Result<Subscribe, ErrorData> {
        let params = events::params_object(params)?;
        let name = events::named(&params)?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v) => v.clone(),
        };
        let filter = Filter::parse(name, Some(&arguments))?;
        let delivery = match params.get("delivery") {
            Some(Value::Object(d)) => d,
            _ => {
                return Err(invalid(
                    "delivery is required: {mode: \"webhook\", url, secret}",
                ));
            }
        };
        match delivery.get("mode").and_then(Value::as_str) {
            Some("webhook") => {}
            other => {
                return Err(ErrorData::new(
                    ErrorCode(events::UNSUPPORTED),
                    format!(
                        "{} is delivered by webhook only: delivery.mode must be \"webhook\"",
                        name.as_str()
                    ),
                    Some(json!({"feature": "deliveryMode", "value": other})),
                ));
            }
        }
        let url = delivery
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("delivery.url is required"))?;
        egress::check_url(url, self.config.policy)
            .map_err(|why| invalid(format!("delivery.url can't be used: {why}")))?;
        let secret = delivery
            .get("secret")
            .and_then(Value::as_str)
            .filter(|s| secret_key(s).is_some())
            .ok_or_else(|| {
                invalid("delivery.secret must be whsec_ and the Base64 of 24 to 64 random bytes")
            })?;
        let cursor = events::cursor_param(&params)?;
        if let Some(c) = &cursor {
            // A malformed one is refused now, not after verifying.
            self.hub.resolve(Some(c))?;
        }
        let c = &self.config;
        let ttl = match params.get("ttlMs") {
            None => c.default_ttl,
            Some(Value::Null) => c.max_ttl,
            Some(v) => match v.as_u64() {
                Some(ms) => Duration::from_millis(ms).clamp(c.min_ttl, c.max_ttl),
                None => return Err(invalid("ttlMs must be a whole number of milliseconds")),
            },
        };
        let owner = vibe_core::keys::hash(caller.key());
        let id = subscription_id(&owner, url, name.as_str(), &arguments);
        Ok(Subscribe {
            owner,
            owner_name: caller.name().to_owned(),
            name,
            filter,
            url: url.to_owned(),
            secret: secret.to_owned(),
            cursor,
            ttl,
            id,
        })
    }

    fn room_for(&self, st: &State, s: &Subscribe) -> Result<(), ErrorData> {
        if st.subs.contains_key(&s.id) {
            return Ok(());
        }
        let theirs = st
            .subs
            .values()
            .filter(|x| x.terms.owner == s.owner)
            .count();
        let (limit, max) = if theirs >= self.config.max_per_owner {
            ("subscriptionsPerKey", self.config.max_per_owner)
        } else if st.subs.len() >= self.config.max_total {
            ("subscriptions", self.config.max_total)
        } else {
            return Ok(());
        };
        Err(ErrorData::new(
            ErrorCode(events::RESOURCE_EXHAUSTED),
            format!("there are {max} webhook subscriptions already: unsubscribe one first"),
            Some(json!({"limit": limit, "max": max})),
        ))
    }

    /// Whether `filter` of `owner`'s hears what's relayed.
    fn relayed(&self, owner: &str, filter: &Filter) -> Filter {
        let mut f = filter.clone();
        f.relayed = self.owners.get().is_some_and(|o| o.hears_relayed(owner));
        f
    }

    /// `events/subscribe`: creates the subscription, or refreshes it.
    pub async fn subscribe(&self, s: Subscribe) -> Result<Value, ErrorData> {
        self.room_for(&lock(&self.state), &s)?;
        let pair = (s.owner.clone(), s.url.clone());
        let now = now_ms();
        let verified = lock(&self.state)
            .verified
            .get(&pair)
            .is_some_and(|until| *until > now);
        if !verified {
            if let Err(reason) = self.verify(&s.id, &s.url, &s.secret).await {
                self.log(&format!(
                    "mcp: key {:?}'s callback at {} wasn't verified: {reason}",
                    s.owner_name,
                    url_host(&s.url)
                ));
                return Err(ErrorData::new(
                    ErrorCode(CALLBACK_ENDPOINT_ERROR),
                    format!("the callback URL wasn't verified: {reason}"),
                    Some(json!({"reason": reason})),
                ));
            }
            lock(&self.state)
                .verified
                .insert(pair, now_ms() + self.config.verified_for.as_millis() as u64);
        }
        let mut st = lock(&self.state);
        self.room_for(&st, &s)?;
        let now = now_ms();
        let expires_ms = now + s.ttl.as_millis() as u64;
        let filter = self.relayed(&s.owner, &s.filter);
        let resolved = match &s.cursor {
            Some(c) => Some(self.hub.resolve(Some(c))?),
            None => None,
        };
        let refreshed = st.subs.contains_key(&s.id);
        let (watermark, truncated, status) = if let Some(sub) = st.subs.get_mut(&s.id) {
            // Live: the cursor goes on as it was. Anything missed since the
            // client's says so.
            if sub.terms.secret != s.secret {
                sub.terms.old_secret = Some(std::mem::replace(&mut sub.terms.secret, s.secret));
                sub.terms.old_until_ms = now + self.config.rotation.as_millis() as u64;
            }
            sub.terms.expires_ms = expires_ms;
            let truncated = match resolved {
                Some((_, true)) => true,
                Some((after, false)) => self.hub.gap_after(after, &filter),
                None => false,
            };
            let status = json!({
                "active": true,
                "lastDeliveryAt": sub.last_delivery_ms.map(iso),
                "lastError": sub.last_error,
            });
            (sub.watermark(), truncated, Some(status))
        } else {
            // New: from the cursor, if it's one this process gave, so what
            // happened since is delivered; or else from now.
            let (scanned, truncated) = match resolved {
                Some((_, true)) => (self.hub.last(), true),
                Some((after, false)) => (after, self.hub.gap_after(after, &filter)),
                None => (self.hub.last(), false),
            };
            st.subs.insert(
                s.id.clone(),
                Sub {
                    terms: Terms {
                        id: s.id.clone(),
                        owner: s.owner.clone(),
                        owner_name: s.owner_name.clone(),
                        url: s.url.clone(),
                        secret: s.secret,
                        old_secret: None,
                        old_until_ms: 0,
                        expires_ms,
                    },
                    name: s.name,
                    filter: s.filter.clone(),
                    scanned,
                    pending: BTreeSet::new(),
                    last_delivery_ms: None,
                    last_error: None,
                },
            );
            (scanned, truncated, None)
        };
        drop(st);
        self.log(&format!(
            "mcp: key {:?} {} {} by webhook to {}",
            s.owner_name,
            if refreshed {
                "refreshed"
            } else {
                "subscribed to"
            },
            s.name.as_str(),
            url_host(&s.url)
        ));
        self.changed();
        let mut result = json!({
            "resultType": "complete",
            "id": s.id,
            "refreshBefore": iso(expires_ms),
            "cursor": self.hub.cursor(watermark),
            "truncated": truncated,
        });
        if let Some(status) = status {
            result["deliveryStatus"] = status;
        }
        Ok(result)
    }

    /// `events/unsubscribe`: ends the subscription, if there's one; either
    /// way, `{}`.
    pub fn unsubscribe(&self, caller: &Caller, params: Option<&Value>) -> Result<Value, ErrorData> {
        let params = events::params_object(params)?;
        let name = events::named(&params)?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(v) => v.clone(),
        };
        let delivery = params.get("delivery").and_then(Value::as_object);
        if let Some(mode) = delivery.and_then(|d| d.get("mode"))
            && mode.as_str() != Some("webhook")
        {
            return Err(invalid("delivery.mode must be \"webhook\""));
        }
        let url = delivery
            .and_then(|d| d.get("url"))
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("delivery.url is required"))?;
        let owner = vibe_core::keys::hash(caller.key());
        let id = subscription_id(&owner, url, name.as_str(), &arguments);
        let gone = lock(&self.state).subs.remove(&id).is_some();
        if gone {
            self.log(&format!(
                "mcp: key {:?} unsubscribed from {} at {}",
                caller.name(),
                name.as_str(),
                url_host(url)
            ));
            self.changed();
        }
        Ok(json!({"resultType": "complete"}))
    }

    /// After subscriptions changed: read, and their owners said.
    fn changed(&self) {
        self.wake.notify_one();
        let owners = {
            let mut st = lock(&self.state);
            let now = st.owners();
            if st.owners.as_ref() == Some(&now) {
                None
            } else {
                st.owners = Some(now.clone());
                Some(now)
            }
        };
        if let (Some(owners), Some(o)) = (owners, self.owners.get()) {
            o.owners_changed(&owners);
        }
    }

    /// Sends a verification to `url`, and checks what comes back: `Err`
    /// says why it failed, as a category.
    async fn verify(&self, id: &str, url: &str, secret: &str) -> Result<(), &'static str> {
        let random = |n: usize| {
            vibe_core::keys::random_bytes()
                .map(|b| hex(&b[..n]))
                .map_err(|_| "connection_refused")
        };
        let challenge = random(24)?;
        let message = format!("msg_verification_{}", random(12)?);
        let body = serde_json::to_vec(&json!({"type": "verification", "challenge": challenge}))
            .expect("a challenge serializes");
        let answer = self
            .post(url, id, &message, &[secret.to_owned()], body)
            .await
            .map_err(Failure::reason)?;
        let echoed = serde_json::from_slice::<Value>(&answer).ok().and_then(|v| {
            v.get("challenge")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        match echoed {
            Some(e) if same_secret(e.as_bytes(), challenge.as_bytes()) => Ok(()),
            _ => Err("challenge_failed"),
        }
    }

    /// Posts one signed message: the answer's body if it was 2xx.
    async fn post(
        &self,
        url: &str,
        id: &str,
        message: &str,
        secrets: &[String],
        body: Vec<u8>,
    ) -> Result<Vec<u8>, Failure> {
        let ts = now_ms() / 1000;
        let headers = [
            ("webhook-id", message.to_owned()),
            ("webhook-timestamp", ts.to_string()),
            ("webhook-signature", sign(secrets, message, ts, &body)),
            ("X-MCP-Subscription-Id", id.to_owned()),
        ];
        let answer =
            egress::post(url, self.config.policy, &headers, body, self.config.timeout).await?;
        if (200..300).contains(&answer.status) {
            Ok(answer.body)
        } else {
            Err(Failure::Status(answer.status))
        }
    }

    async fn run(self: Arc<Self>) {
        let mut newest = self.hub.watch_newest();
        let mut keys = self.hub.watch_keys();
        let mut sweep = tokio::time::interval(self.config.sweep_every);
        self.check_owners().await;
        self.changed();
        loop {
            self.scan();
            tokio::select! {
                () = self.stopped() => return,
                changed = newest.changed() => if changed.is_err() { return },
                () = self.wake.notified() => {}
                changed = keys.changed() => if changed.is_ok() {
                    self.check_owners().await;
                },
                _ = sweep.tick() => self.sweep(),
            }
        }
    }

    /// Reads on for every subscription, and sends what's new.
    fn scan(self: &Arc<Self>) {
        let mut sends = Vec::new();
        let mut dropped = Vec::new();
        {
            let mut st = lock(&self.state);
            let now = now_ms();
            for sub in st.subs.values_mut() {
                if sub.terms.expires_ms <= now {
                    continue;
                }
                let filter = self.relayed(&sub.terms.owner, &sub.filter);
                loop {
                    let read = self.hub.read(sub.scanned, &filter, 256);
                    for o in &read.events {
                        if sub.pending.len() >= MAX_PENDING {
                            dropped.push(sub.terms.owner_name.clone());
                            continue;
                        }
                        sub.pending.insert(o.seq);
                        sends.push((sub.terms.id.clone(), o.clone()));
                    }
                    sub.scanned = read.cursor;
                    if !read.has_more {
                        break;
                    }
                }
            }
        }
        for name in dropped.into_iter().collect::<HashSet<_>>() {
            self.log(&format!(
                "mcp: key {name:?}'s webhook has {MAX_PENDING} events waiting: giving up newer ones"
            ));
        }
        for (id, o) in sends {
            tokio::spawn(Arc::clone(self).deliver(id, o));
        }
    }

    /// What to post for `o` to subscription `id` now, if it's still there.
    fn prepare(&self, id: &str, o: &Occurrence) -> Option<(String, Vec<String>, String, Vec<u8>)> {
        let st = lock(&self.state);
        let sub = st.subs.get(id)?;
        let now = now_ms();
        if sub.terms.expires_ms <= now {
            return None;
        }
        let event_id = format!(
            "evt_{}_{}",
            &id[4..16],
            self.hub.cursor(o.seq).replace('-', "_")
        );
        let body = json!({
            "eventId": event_id,
            "name": sub.name.as_str(),
            "timestamp": o.timestamp,
            "data": o.data(),
            "cursor": self.hub.cursor(sub.watermark_with(o.seq)),
        });
        let body = serde_json::to_vec(&body).expect("an event serializes");
        if body.len() > MAX_BODY {
            return None;
        }
        Some((sub.terms.url.clone(), sub.secrets(now), event_id, body))
    }

    fn note(&self, id: &str, outcome: Result<(), Failure>) {
        if let Some(sub) = lock(&self.state).subs.get_mut(id) {
            match outcome {
                Ok(()) => {
                    sub.last_delivery_ms = Some(now_ms());
                    sub.last_error = None;
                }
                Err(f) => sub.last_error = Some(f.reason()),
            }
        }
    }

    /// Delivers `o` to subscription `id`: tries until it's taken, it's
    /// refused for good (410, 413), the tries run out, or the subscription
    /// goes.
    async fn deliver(self: Arc<Self>, id: String, o: Occurrence) {
        let tries = self.config.retries.len() + 1;
        for attempt in 0..tries {
            if attempt > 0 {
                tokio::select! {
                    () = self.stopped() => break,
                    () = tokio::time::sleep(self.config.retries[attempt - 1]) => {}
                }
            }
            let Ok(_turn) = self.sending.acquire().await else {
                break;
            };
            if *self.stop.borrow() {
                break;
            }
            // Made again for each try: its cursor may have moved on.
            let Some((url, secrets, event_id, body)) = self.prepare(&id, &o) else {
                break;
            };
            let sent = self.post(&url, &id, &event_id, &secrets, body).await;
            let outcome = sent.map(|_| ());
            self.note(&id, outcome);
            match outcome {
                Ok(()) => break,
                Err(Failure::Status(410 | 413)) => break,
                Err(_) if attempt + 1 == tries => {
                    let name = lock(&self.state)
                        .subs
                        .get(&id)
                        .map(|s| s.terms.owner_name.clone());
                    if let Some(name) = name {
                        self.log(&format!(
                            "mcp: gave up delivering an event to key {name:?}'s webhook at {} after {tries} tries",
                            url_host(&url)
                        ));
                    }
                }
                Err(_) => {}
            }
        }
        if let Some(sub) = lock(&self.state).subs.get_mut(&id) {
            sub.pending.remove(&o.seq);
        }
    }

    /// Drops what's expired.
    fn sweep(&self) {
        let now = now_ms();
        let mut gone = Vec::new();
        {
            let mut st = lock(&self.state);
            st.subs.retain(|_, s| {
                let keep = s.terms.expires_ms > now;
                if !keep {
                    gone.push((s.terms.owner_name.clone(), s.name.as_str()));
                }
                keep
            });
            st.verified.retain(|_, until| *until > now);
            for s in st.subs.values_mut() {
                if s.terms.old_until_ms <= now {
                    s.terms.old_secret = None;
                }
            }
        }
        for (owner, name) in &gone {
            self.log(&format!(
                "mcp: key {owner:?}'s webhook subscription to {name} expired"
            ));
        }
        if !gone.is_empty() {
            self.changed();
        }
    }

    /// Ends the subscriptions of keys that are gone.
    async fn check_owners(&self) {
        let Some(o) = self.owners.get().cloned() else {
            return;
        };
        let owners = self.owners();
        let held = tokio::task::spawn_blocking(move || {
            owners
                .into_iter()
                .map(|owner| {
                    let holds = o.holds(&owner);
                    (owner, holds)
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        let gone: HashSet<String> = held
            .into_iter()
            .filter(|(_, holds)| !holds)
            .map(|(owner, _)| owner)
            .collect();
        if gone.is_empty() {
            return;
        }
        let mut ended: HashMap<String, usize> = HashMap::new();
        lock(&self.state).subs.retain(|_, s| {
            let keep = !gone.contains(&s.terms.owner);
            if !keep {
                *ended.entry(s.terms.owner_name.clone()).or_default() += 1;
            }
            keep
        });
        lock(&self.state)
            .verified
            .retain(|(owner, _), _| !gone.contains(owner));
        for (name, n) in ended {
            self.log(&format!(
                "mcp: key {name:?} is gone: ended its {n} webhook subscription{}",
                if n == 1 { "" } else { "s" }
            ));
        }
        self.changed();
    }
}

impl Drop for Webhooks {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

/// A URL's host, for the log: its path may be a secret of its own.
fn url_host(url: &str) -> String {
    url.parse::<http::Uri>()
        .ok()
        .and_then(|u| u.host().map(str::to_owned))
        .unwrap_or_else(|| "?".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_are_standard_webhooks() {
        // Standard Webhooks' own example.
        let secret = "whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw".to_owned();
        assert_eq!(
            sign(
                std::slice::from_ref(&secret),
                "msg_p5jXN8AQM9LWM0D4loKWxJek",
                1614265330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
        // Two while a secret's replaced, space-separated.
        let other = format!("whsec_{}", STANDARD.encode([7u8; 32]));
        let both = sign(&[secret, other], "msg_1", 1, b"{}");
        let parts: Vec<&str> = both.split(' ').collect();
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.starts_with("v1,")));
    }

    #[test]
    fn secrets_are_whsec_and_24_to_64_bytes() {
        for n in [24, 32, 64] {
            assert!(secret_key(&format!("whsec_{}", STANDARD.encode(vec![1u8; n]))).is_some());
        }
        for bad in [
            format!("whsec_{}", STANDARD.encode([1u8; 23])),
            format!("whsec_{}", STANDARD.encode([1u8; 65])),
            STANDARD.encode([1u8; 32]),
            "whsec_not base64!".to_owned(),
            "whsec_".to_owned(),
        ] {
            assert!(secret_key(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn ids_are_the_same_for_the_same_subscription_whatever_the_order_of_its_arguments() {
        let a = json!({"host": "vps", "state": ["idle", "question"], "project": "app"});
        let b = json!({"project": "app", "state": ["idle", "question"], "host": "vps"});
        assert_eq!(
            canonical(&a),
            r#"{"host":"vps","project":"app","state":["idle","question"]}"#
        );
        assert_eq!(
            canonical(&json!({"a": {"z": 1, "b": [true, null]}})),
            r#"{"a":{"b":[true,null],"z":1}}"#
        );
        let id = |owner, url, args| subscription_id(owner, url, "agent.turn_ended", args);
        assert_eq!(
            id("o", "https://x.example/h", &a),
            id("o", "https://x.example/h", &b)
        );
        assert!(id("o", "https://x.example/h", &a).starts_with("sub_"));
        assert_eq!(id("o", "https://x.example/h", &a).len(), 36);
        // Any part different is another.
        assert_ne!(
            id("o", "https://x.example/h", &a),
            id("p", "https://x.example/h", &a)
        );
        assert_ne!(
            id("o", "https://x.example/h", &a),
            id("o", "https://x.example/i", &a)
        );
        assert_ne!(
            id("o", "https://x.example/h", &a),
            id("o", "https://x.example/h", &json!({}))
        );
        assert_ne!(
            subscription_id("o", "u", "agent.started", &a),
            subscription_id("o", "u", "agent.deleted", &a)
        );
    }
}
