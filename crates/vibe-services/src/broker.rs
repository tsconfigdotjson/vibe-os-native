//! The broker: its host's one holder of each service's login, and what every
//! call by that host's agents passes through. It knows nothing of which kind of
//! agent is calling: policy is on the service's own tool names, so it's the same
//! for all. Every `vibed` runs one for its own agents, and none needs another.
//!
//! **The lifecycle** (PRD: Shared MCP services). A service is `disconnected`,
//! `connected`, `refreshing`, `needs_reconnect` (a refresh was refused) or
//! `revoked` (a freshly refreshed token, or a key, was turned down). Refreshes
//! go one at a time per service, and a rotated token is persisted **before**
//! it's used. A refresh runs on a task of its own that its callers wait on, so
//! a shim going away can't cut one short: only a real crash can come between the
//! service rotating a token and the write, and then the old token is what's
//! kept, and is replayed within the service's replay grace. When
//! auth fails, tools still list, from what was last seen, and calls get a tool
//! error saying what to do. They work again after Reconnect, with nothing to
//! do in the agent.
//!
//! **Signing in** is in two halves, so the app can drive the browser for any
//! host: this host registers its client, makes the PKCE verifier and the state,
//! and keeps them; the app opens the page; and this host makes the exchange
//! with the code that comes back. Neither the verifier nor a token goes out.
//!
//! **Upstream sessions** belong here, one per (agent, service), so a shim that
//! reconnects, or a VM that wakes, carries on. A session the service has
//! forgotten (404) is made again and the call retried.
//!
//! Nothing secret leaves: not to a shim, a log, the state file or an error.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::fs;
use std::io::{self, BufRead, Write as _};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use vibe_core::model::{ServiceRole, ServiceState};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, ServiceAccess, ServiceAuditEntry, ServiceAuditList,
    ServiceAuthorization, ServiceFollowParams, ServiceInfo, ServiceList, ServiceSetParams,
};
use vibe_core::time::rfc3339;
use vibe_relay::MAX_MESSAGE;
use vibe_relay::proxy::tool_error;

use crate::http::{Http, Reply};
use crate::oauth::{self, Authorize, CALLBACK_SCHEME, Metadata, REDIRECT_URI, TokenError};
use crate::registry::Service;
use crate::secrets::{Secret, SecretStore};
use crate::util::{base64url, query_of};

/// A token this close to running out is refreshed first.
const REFRESH_EARLY: i64 = 60;

/// How long a service gets to start answering a call, and to go on: its SSE
/// keep-alives count.
const CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long a catalog is taken as current.
const CATALOG_FRESH: Duration = Duration::from_secs(60);

/// How long an upstream session is kept after its last use.
const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);

/// How long a sign-in may take.
const PENDING_FOR: Duration = Duration::from_secs(15 * 60);

/// How long after a follower last waited in vain its calls fail at once, rather
/// than each waiting again for an owner that's plainly away.
const ASK_AGAIN_AFTER: Duration = Duration::from_secs(60);

/// How often a waiting follower looks to see if it's been given a token.
const TOKEN_LOOK: Duration = Duration::from_millis(50);

/// How long a follower's call waits by default to be given a token.
pub const TOKEN_WAIT: Duration = Duration::from_secs(10);

/// What this client offers a service.
const PROTOCOL: &str = "2025-06-18";

/// Whose upstream session the catalog is read in.
const CATALOG_AGENT: &str = "(catalog)";

/// How many pages of tools a catalog may have, should a service never stop.
const MAX_PAGES: usize = 50;

/// How many of one shim's messages may be under way at once. It may have many
/// calls out, but no shim can make the broker hold without limit.
const MAX_IN_FLIGHT: usize = 32;

/// The audit entries kept to show, and how big their file gets before the older
/// half of its life is set aside.
const AUDIT_KEPT: usize = 500;
const AUDIT_FILE_MAX: u64 = 5 << 20;

/// What the broker needs from the daemon around it.
#[derive(Clone)]
pub struct Deps {
    /// Seconds since the epoch.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// 256 random bits.
    pub random: Arc<dyn Fn() -> io::Result<[u8; 32]> + Send + Sync>,
    /// A service's state, settings or tools changed.
    pub changed: Arc<dyn Fn(&ServiceInfo) + Send + Sync>,
    /// One line per call: agent, service, tool, outcome and time, and never
    /// its arguments.
    pub audit: Arc<dyn Fn(&str) + Send + Sync>,
    /// Which kind of host this is, for what agents are told to do.
    pub place: Place,
    /// How long a follower's call waits to be given a token.
    pub token_wait: Duration,
}

/// Where this broker runs, which is where its login gets mended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Mac,
    LinuxHost,
}

impl Place {
    /// Where its logins are, for people.
    fn store(self) -> &'static str {
        match self {
            Place::Mac => "the Keychain",
            Place::LinuxHost => "this host's login file",
        }
    }
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What's kept of a service on disk. Nothing secret: tools and settings.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Saved {
    #[serde(default = "yes")]
    enabled: bool,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    denied_tools: Vec<String>,
    /// Claude Code runs its tools without asking, for agents started from here
    /// on. Off by default, so its own prompts govern writes.
    #[serde(default)]
    auto_allow: bool,
    #[serde(default = "disconnected")]
    state: ServiceState,
    /// This host's part in the login. A host that's never been told is a
    /// follower of nothing, so disconnected.
    #[serde(default = "follower")]
    role: ServiceRole,
    /// Who owns the login, for people, if it's another host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
    /// A follower whose token was refused or ran out, waiting to be given another.
    #[serde(default)]
    wants_token: bool,
    /// Where the Mac has the login: a host's name. Only the Mac keeps this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    assigned: Option<String>,
    /// The Mac disconnected it, so no host that still says it owns it is believed.
    #[serde(default)]
    released: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detail_at: Option<i64>,
    /// Tools as last seen, whole: what's refused is filtered as they're given.
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    /// The registered client was refused, so the next sign-in registers anew.
    #[serde(default)]
    client_stale: bool,
}

fn yes() -> bool {
    true
}

fn disconnected() -> ServiceState {
    ServiceState::Disconnected
}

fn follower() -> ServiceRole {
    ServiceRole::Follower
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            enabled: true,
            read_only: false,
            denied_tools: Vec::new(),
            auto_allow: false,
            state: ServiceState::Disconnected,
            role: ServiceRole::Follower,
            owner: None,
            wants_token: false,
            assigned: None,
            released: false,
            detail: None,
            detail_at: None,
            tools: Vec::new(),
            instructions: None,
            client_stale: false,
        }
    }
}

/// Why a token can't be had now.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AuthFailure {
    NotConnected,
    /// Only signing in again can fix it.
    NeedsReconnect,
    /// This host's token is no good, and its owner can't be reached for another.
    OwnerUnreachable,
    /// It may pass: the service or this host's login store can't be reached.
    Unavailable(String),
}

/// One service: its login and how it stands.
struct Conn {
    def: Service,
    place: Place,
    /// The login as last read from, or written to, the store.
    secret: Mutex<Option<Secret>>,
    /// Whether `secret` has been read: a locked Keychain can make that wait.
    loaded: AtomicBool,
    /// Held across a refresh, so there's one at a time.
    refreshing: tokio::sync::Mutex<()>,
    saved: Mutex<Saved>,
    /// When the catalog was last read from the service.
    catalog_read: Mutex<Option<Instant>>,
    /// Held across reading it.
    catalog_gate: tokio::sync::Mutex<()>,
    /// As a follower: what it's learned of the token it holds.
    following: Mutex<Following>,
}

#[derive(Debug, Default)]
struct Following {
    /// The token the service turned down, which is no use until replaced.
    rejected: Option<String>,
    /// When a call last waited for a token and none came.
    gave_up: Option<Instant>,
}

impl fmt::Debug for Conn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("service", &self.def.name)
            .finish_non_exhaustive()
    }
}

impl Conn {
    fn endpoint(&self) -> String {
        let saved = lock(&self.saved);
        match (&self.def.read_only_endpoint, saved.read_only) {
            (Some(read_only), true) => read_only.clone(),
            _ => self.def.endpoint.clone(),
        }
    }

    /// Whichever host owns it, signing in again happens in the app on the Mac.
    fn needs_reconnect_text(&self) -> String {
        format!(
            "{} needs reconnecting on the Mac (VibeOS → Settings → Services → Reconnect).",
            self.def.title
        )
    }

    fn not_connected_text(&self) -> String {
        format!(
            "{} isn't connected on the Mac (VibeOS → Settings → Services → Connect).",
            self.def.title
        )
    }

    fn owner_unreachable_text(&self) -> String {
        let owner = lock(&self.saved)
            .owner
            .clone()
            .unwrap_or_else(|| "the host that owns it".to_owned());
        format!(
            "{} has no current access token here: its login is on {owner}, which can't be reached \
             from here right now. Calls work again once it's back.",
            self.def.title
        )
    }

    fn off_text(&self) -> String {
        let title = &self.def.title;
        match self.place {
            Place::Mac => {
                format!("{title} is turned off on the Mac (VibeOS → Settings → Services).")
            }
            Place::LinuxHost => {
                format!(
                    "{title} is turned off on this host (`vibe service on {}` here, or VibeOS → \
                     Settings → Services on the Mac).",
                    self.def.name
                )
            }
        }
    }

    fn unavailable_text(&self, why: &str) -> String {
        let here = match self.place {
            Place::Mac => "the Mac",
            Place::LinuxHost => "this host",
        };
        format!(
            "{} can't be reached from {here} now: {why}.",
            self.def.title
        )
    }

    fn failure_text(&self, failure: &AuthFailure) -> String {
        match failure {
            AuthFailure::NotConnected => self.not_connected_text(),
            AuthFailure::NeedsReconnect => self.needs_reconnect_text(),
            AuthFailure::OwnerUnreachable => self.owner_unreachable_text(),
            AuthFailure::Unavailable(why) => self.unavailable_text(why),
        }
    }

    fn is_denied(&self, tool: &str) -> bool {
        self.is_denied_in(&lock(&self.saved), tool)
    }

    fn is_denied_in(&self, saved: &Saved, tool: &str) -> bool {
        self.def.denied_tools.iter().any(|t| t == tool)
            || saved.denied_tools.iter().any(|t| t == tool)
    }
}

/// An upstream session.
#[derive(Debug, Clone)]
struct UpstreamSession {
    id: Option<String>,
    protocol: String,
}

#[derive(Debug, Default)]
struct SessionCell {
    session: tokio::sync::Mutex<Option<UpstreamSession>>,
    used: Mutex<Option<Instant>>,
}

/// A sign-in under way.
struct Pending {
    state: String,
    verifier: String,
    client_id: String,
    md: Metadata,
    resource: String,
    scopes: Vec<String>,
    at: Instant,
}

/// Why a call to a service failed.
#[derive(Debug)]
enum CallError {
    Auth(AuthFailure),
    Unavailable(String),
}

/// What the broker serves: every supported service.
pub struct Services {
    /// Itself, for what must outlive whoever asked for it: a refresh.
    me: Weak<Services>,
    conns: Vec<Arc<Conn>>,
    http: Http,
    store: Arc<dyn SecretStore>,
    state_file: PathBuf,
    /// One write of the state file at a time.
    state_lock: Mutex<()>,
    deps: Deps,
    sessions: Mutex<HashMap<(String, String), Arc<SessionCell>>>,
    pending: Mutex<HashMap<String, Pending>>,
    /// The last calls, for the app, and where they're kept: on this host, as
    /// its calls are its own.
    audit: Mutex<VecDeque<ServiceAuditEntry>>,
    audit_file: PathBuf,
}

impl fmt::Debug for Services {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Services")
            .field("services", &self.conns)
            .finish_non_exhaustive()
    }
}

fn not_found(service: &str) -> ErrorObject {
    ErrorObject::new(
        ErrorCode::NotFound,
        format!("VibeOS has no shared service {service}"),
    )
}

fn failed(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::CommandFailed, message)
}

impl Services {
    /// Picks up from the state file. Logins are read later, never here, as
    /// `security` may take its time and a daemon must serve its socket first:
    /// [`Services::load_logins`] in the background, or on first use.
    ///
    /// A state file that can't be read has lost the owner's policy: its denied
    /// tools and read-only choices. So every service starts turned off and says
    /// why, rather than quietly coming up with no policy at all.
    pub fn new(
        services: Vec<Service>,
        store: Arc<dyn SecretStore>,
        state_file: PathBuf,
        deps: Deps,
    ) -> Arc<Services> {
        let (mut all, unreadable) = read_state(&state_file);
        if let Some(why) = &unreadable {
            (deps.audit)(&format!(
                "services: {} can't be read ({why}): every service is off until it's turned on",
                state_file.display()
            ));
        }
        let audit_file = state_file.with_file_name("services-audit.log");
        let audit = read_audit(&audit_file);
        let now = (deps.clock)();
        let conns = services
            .into_iter()
            .map(|def| {
                let mut saved = all.remove(&def.name).unwrap_or_default();
                // A refresh is never under way as a daemon starts.
                if saved.state == ServiceState::Refreshing {
                    saved.state = ServiceState::Connected;
                }
                if unreadable.is_some() {
                    saved.enabled = false;
                    saved.detail = Some(settings_lost_text(&def, deps.place));
                    saved.detail_at = Some(now);
                }
                Arc::new(Conn {
                    def,
                    place: deps.place,
                    secret: Mutex::new(None),
                    loaded: AtomicBool::new(false),
                    refreshing: tokio::sync::Mutex::new(()),
                    saved: Mutex::new(saved),
                    catalog_read: Mutex::new(None),
                    catalog_gate: tokio::sync::Mutex::new(()),
                    following: Mutex::default(),
                })
            })
            .collect();
        let services = Arc::new_cyclic(|me| Services {
            me: me.clone(),
            conns,
            http: Http::new(),
            store,
            state_file,
            state_lock: Mutex::new(()),
            deps,
            sessions: Mutex::default(),
            pending: Mutex::default(),
            audit: Mutex::new(audit),
            audit_file,
        });
        // Off has to outlast this run. Until it's written the bad file stays
        // where it is, so a restart finds it and comes up off again; nothing but
        // this write takes its place.
        if unreadable.is_some() {
            set_aside(&services.state_file);
            if let Err(e) = services.persist() {
                (services.deps.audit)(&format!(
                    "services: can't keep {}: {e}",
                    services.state_file.display()
                ));
            }
        }
        services
    }

    /// Reads every login, so what's shown is right before anything's asked of
    /// it. It's fine for this to fail: each is tried again as it's needed.
    pub async fn load_logins(&self) {
        for conn in &self.conns {
            let _ = self.load_secret(conn).await;
        }
    }

    /// The last calls to this host's services, oldest first.
    pub fn audit(&self) -> ServiceAuditList {
        ServiceAuditList {
            entries: lock(&self.audit).iter().cloned().collect(),
        }
    }

    fn record_call(&self, entry: ServiceAuditEntry) {
        (self.deps.audit)(&format!(
            "services: agent={} service={} tool={} outcome={} ms={}",
            entry.agent, entry.service, entry.tool, entry.outcome, entry.ms
        ));
        if let Err(e) = append_audit(&self.audit_file, &entry) {
            (self.deps.audit)(&format!(
                "services: can't keep the audit line in {}: {e}",
                self.audit_file.display()
            ));
        }
        let mut kept = lock(&self.audit);
        if kept.len() >= AUDIT_KEPT {
            kept.pop_front();
        }
        kept.push_back(entry);
    }

    /// Whether Claude Code's agents are to run `service`'s tools unasked.
    pub fn auto_allowed(&self) -> Vec<String> {
        self.conns
            .iter()
            .filter(|c| lock(&c.saved).auto_allow)
            .map(|c| c.def.name.clone())
            .collect()
    }

    fn conn(&self, service: &str) -> Option<&Arc<Conn>> {
        self.conns.iter().find(|c| c.def.name == service)
    }

    fn require(&self, service: &str) -> Result<&Arc<Conn>, ErrorObject> {
        self.conn(service).ok_or_else(|| not_found(service))
    }

    fn info(&self, conn: &Conn) -> ServiceInfo {
        let saved = lock(&conn.saved);
        ServiceInfo {
            name: conn.def.name.clone(),
            title: conn.def.title.clone(),
            state: saved.state,
            role: saved.role,
            owner: saved.owner.clone(),
            access_until: lock(&conn.secret)
                .as_ref()
                .and_then(|s| s.expires_at)
                .map(rfc3339),
            wants_token: saved.wants_token,
            enabled: saved.enabled,
            read_only: saved.read_only,
            denied_tools: saved.denied_tools.clone(),
            auto_allow: saved.auto_allow,
            tools: if saved.state == ServiceState::Disconnected || !saved.enabled {
                0
            } else {
                saved
                    .tools
                    .iter()
                    .filter(|t| !conn.is_denied_in(&saved, t["name"].as_str().unwrap_or_default()))
                    .count() as u32
            },
            detail: saved.detail.clone(),
            detail_at: saved.detail_at.map(rfc3339),
        }
    }

    /// Keeps the state file, whole and durably: it's the owner's policy, so a
    /// crash must leave the old one or the new one, and never half of either.
    fn persist(&self) -> io::Result<()> {
        let _one = lock(&self.state_lock);
        let all: HashMap<&str, Saved> = self
            .conns
            .iter()
            .map(|c| (c.def.name.as_str(), lock(&c.saved).clone()))
            .collect();
        let bytes = serde_json::to_vec_pretty(&all).map_err(io::Error::other)?;
        let tmp = self.state_file.with_extension("new");
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &self.state_file)?;
        if let Some(dir) = self.state_file.parent() {
            fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    fn changed(&self, conn: &Conn) {
        // Only what the service last said is at stake here, not a choice of the
        // owner's, which `set` keeps for itself. It's written again next time.
        if let Err(e) = self.persist() {
            (self.deps.audit)(&format!(
                "services: can't keep {}: {e}",
                self.state_file.display()
            ));
        }
        (self.deps.changed)(&self.info(conn));
    }

    /// Makes a change to what's kept of a service, and says so if it did.
    fn update(&self, conn: &Conn, f: impl FnOnce(&mut Saved)) {
        let was = {
            let mut saved = lock(&conn.saved);
            let was = serde_json::to_value(&*saved).ok();
            f(&mut saved);
            (was != serde_json::to_value(&*saved).ok()).then_some(())
        };
        if was.is_some() {
            self.changed(conn);
        }
    }

    /// Text for people, with no token in it, whatever a service said.
    fn scrub(&self, conn: &Conn, text: &str) -> String {
        self.scrub_also(conn, text, &[])
    }

    /// The same, with `also` taken out too: what a sign-in has in hand. Taken
    /// out first and cut short after, so a cut can't leave part of one behind.
    fn scrub_also(&self, conn: &Conn, text: &str, also: &[&str]) -> String {
        let mut text = text.to_owned();
        if let Some(secret) = &*lock(&conn.secret) {
            text = text.replace(&secret.access, "[redacted]");
            if let Some(r) = &secret.refresh {
                text = text.replace(r, "[redacted]");
            }
        }
        for secret in also.iter().filter(|s| !s.is_empty()) {
            text = text.replace(secret, "[redacted]");
        }
        text.chars().take(300).collect()
    }

    fn note(&self, conn: &Conn, state: ServiceState, detail: Option<String>) {
        let at = (self.deps.clock)();
        let detail = detail.map(|d| self.scrub(conn, &d));
        self.update(conn, |s| {
            s.state = state;
            if detail.is_some() {
                s.detail_at = Some(at);
            }
            if let Some(d) = detail {
                s.detail = Some(d);
            }
        });
    }

    // ---- The socket API.

    /// The supported services, and how each stands.
    pub fn list(&self) -> ServiceList {
        ServiceList {
            services: self.conns.iter().map(|c| self.info(c)).collect(),
        }
    }

    /// The first half of connecting or reconnecting. What's connected stays
    /// until [`Services::complete`].
    pub async fn connect(&self, service: &str) -> Result<ServiceAuthorization, ErrorObject> {
        let conn = self.require(service)?;
        let md = oauth::discover(&self.http, &conn.def.endpoint)
            .await
            .map_err(|e| failed(format!("Can't start signing in to {}: {e}", conn.def.title)))?;
        let (stale, lost) = {
            let saved = lock(&conn.saved);
            (
                saved.client_stale,
                matches!(
                    saved.state,
                    ServiceState::NeedsReconnect | ServiceState::Revoked
                ),
            )
        };
        // A sign-in given up on leaves a client that's still good, and this
        // shouldn't make another each time the button's pressed.
        let abandoned = lock(&self.pending)
            .get(service)
            .filter(|p| p.at.elapsed() < PENDING_FOR)
            .map(|p| p.client_id.clone());
        let known = self
            .load_secret(conn)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.client_id);
        // Reconnecting starts afresh: the client that lost its login may be why.
        let client_id = match (
            abandoned.filter(|_| !stale),
            known.filter(|_| !stale && !lost),
        ) {
            (Some(id), _) | (None, Some(id)) => id,
            (None, None) => oauth::register(&self.http, &md).await.map_err(|e| {
                failed(format!("Can't start signing in to {}: {e}", conn.def.title))
            })?,
        };
        let random = |what: &str| {
            (self.deps.random)()
                .map(|b| base64url(&b))
                .map_err(|e| failed(format!("Can't make {what}: {e}")))
        };
        let verifier = random("a PKCE verifier")?;
        let state = random("a state")?;
        let url = oauth::authorize_url(Authorize {
            md: &md,
            client_id: &client_id,
            verifier: &verifier,
            state: &state,
            resource: &conn.def.endpoint,
            scopes: &conn.def.scopes,
        });
        let mut pending = lock(&self.pending);
        pending.retain(|_, p| p.at.elapsed() < PENDING_FOR);
        pending.insert(
            service.to_owned(),
            Pending {
                state,
                verifier,
                client_id,
                md,
                resource: conn.def.endpoint.clone(),
                scopes: conn.def.scopes.clone(),
                at: Instant::now(),
            },
        );
        Ok(ServiceAuthorization {
            service: service.to_owned(),
            url,
            callback_scheme: CALLBACK_SCHEME.to_owned(),
        })
    }

    /// The second half: the URL the sign-in ended on.
    pub async fn complete(
        self: &Arc<Self>,
        service: &str,
        callback_url: &str,
    ) -> Result<ServiceInfo, ErrorObject> {
        let conn = self.require(service)?;
        let title = &conn.def.title;
        if !callback_url.starts_with(REDIRECT_URI) {
            return Err(invalid("that isn't where a sign-in ends"));
        }
        let query = query_of(callback_url);
        let get = |name: &str| {
            query
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        let pending = lock(&self.pending)
            .remove(service)
            .filter(|p| p.at.elapsed() < PENDING_FOR)
            .ok_or_else(|| {
                ErrorObject::new(
                    ErrorCode::Conflict,
                    format!("No sign-in to {title} is under way; start again"),
                )
            })?;
        if get("state") != Some(&pending.state) {
            return Err(invalid("that sign-in isn't the one under way; start again"));
        }
        match (get("iss"), pending.md.iss_in_redirect) {
            (Some(iss), _) if iss != pending.md.issuer => {
                return Err(invalid(format!(
                    "that sign-in came from {iss}, not {}; start again",
                    pending.md.issuer
                )));
            }
            (None, true) => {
                return Err(invalid(
                    "that sign-in doesn't say who it came from; start again",
                ));
            }
            _ => {}
        }
        let in_hand = [
            pending.verifier.as_str(),
            pending.state.as_str(),
            get("code").unwrap_or_default(),
        ];
        if let Some(error) = get("error") {
            let said = get("error_description").unwrap_or(error);
            return Err(failed(self.scrub_also(
                conn,
                &format!("{title} didn't sign you in: {said}"),
                &in_hand,
            )));
        }
        let code = get("code").ok_or_else(|| invalid("that sign-in gave no code"))?;
        let tokens = oauth::token(
            &self.http,
            &pending.md.token_endpoint,
            &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("client_id", &pending.client_id),
                ("redirect_uri", REDIRECT_URI),
                ("code_verifier", &pending.verifier),
                ("resource", &pending.resource),
            ],
        )
        .await;
        let tokens = match tokens {
            Ok(t) => t,
            Err(TokenError::Refused { code, detail }) => {
                if code == "invalid_client" {
                    self.update(conn, |s| s.client_stale = true);
                }
                let tail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                return Err(failed(self.scrub_also(
                    conn,
                    &format!("{title} refused the sign-in ({code}){tail}; start again"),
                    &in_hand,
                )));
            }
            Err(TokenError::Unavailable(why)) => {
                return Err(failed(self.scrub_also(
                    conn,
                    &format!("Can't finish signing in to {title}: {why}"),
                    &in_hand,
                )));
            }
        };
        let now = (self.deps.clock)();
        let secret = Secret {
            access: tokens.access,
            refresh: tokens.refresh,
            obtained_at: Some(now),
            expires_at: tokens.expires_in.map(|s| now + s),
            client_id: Some(pending.client_id),
            token_endpoint: Some(pending.md.token_endpoint),
            revocation_endpoint: pending.md.revocation_endpoint,
            scopes: tokens.scope.map_or(pending.scopes, |s| {
                s.split_whitespace().map(str::to_owned).collect()
            }),
        };
        self.connected(conn, secret).await?;
        Ok(self.info(conn))
    }

    /// A new login: kept whole, replacing the old one, then in use. Signing in
    /// here is what makes this host its owner.
    async fn connected(
        self: &Arc<Self>,
        conn: &Arc<Conn>,
        secret: Secret,
    ) -> Result<(), ErrorObject> {
        // Under the refresh lock, from before the write: a refresh in flight
        // must finish first, or it would put the old login's tokens back over
        // this one.
        let refreshing = conn.refreshing.lock().await;
        self.write_secret(conn, secret.clone())
            .await
            .map_err(|e| failed(format!("Can't keep the login for {}: {e}", conn.def.title)))?;
        self.hold_login(conn, Some(secret.clone()));
        drop(refreshing);
        self.forget_sessions(&conn.def.name);
        *lock(&conn.catalog_read) = None;
        self.update(conn, |s| {
            s.state = ServiceState::Connected;
            s.role = ServiceRole::Owner;
            s.owner = None;
            s.wants_token = false;
            s.detail = None;
            s.detail_at = None;
            s.client_stale = false;
        });
        *lock(&conn.following) = Following::default();
        // Agents see its tools soon: they ask every few minutes.
        let services = self.clone();
        let conn = conn.clone();
        tokio::spawn(async move {
            let _ = services.catalog(&conn).await;
        });
        Ok(())
    }

    /// Forgets the login, here and, where it can, at the service.
    pub async fn disconnect(&self, service: &str) -> Result<ServiceInfo, ErrorObject> {
        let conn = self.require(service)?;
        let store = self.store.clone();
        let name = conn.def.name.clone();
        let (role, owner) = {
            let saved = lock(&conn.saved);
            (saved.role, saved.owner.clone())
        };
        if role == ServiceRole::Follower && owner.is_some() {
            return Err(ErrorObject::new(
                ErrorCode::Conflict,
                format!(
                    "{}'s login is on {}: disconnect it in VibeOS → Settings → Services on the Mac",
                    conn.def.title,
                    owner.unwrap_or_default()
                ),
            ));
        }
        // As in `connected`: a refresh in flight mustn't write it back after.
        let refreshing = conn.refreshing.lock().await;
        tokio::task::spawn_blocking(move || store.delete(&name))
            .await
            .map_err(|e| failed(e.to_string()))?
            .map_err(|e| failed(format!("Can't forget the login: {e}")))?;
        let gone = self.hold_login(conn, None);
        drop(refreshing);
        self.forget_sessions(service);
        lock(&self.pending).remove(service);
        *lock(&conn.catalog_read) = None;
        self.update(conn, |s| {
            s.state = ServiceState::Disconnected;
            s.role = ServiceRole::Follower;
            s.owner = None;
            s.wants_token = false;
            s.detail = None;
            s.detail_at = None;
            s.tools.clear();
            s.instructions = None;
        });
        if let Some(gone) = gone
            && let Some(endpoint) = &gone.revocation_endpoint
        {
            let token = gone.refresh.as_deref().unwrap_or(&gone.access);
            oauth::revoke(&self.http, endpoint, token, gone.client_id.as_deref()).await;
        }
        Ok(self.info(conn))
    }

    /// Changes the settings given.
    pub fn set(&self, p: ServiceSetParams) -> Result<ServiceInfo, ErrorObject> {
        let conn = self.require(&p.service)?;
        if p.read_only == Some(true) && conn.def.read_only_endpoint.is_none() {
            return Err(invalid(format!(
                "{} has no read-only endpoint",
                conn.def.title
            )));
        }
        let before = {
            let mut saved = lock(&conn.saved);
            let before = saved.clone();
            if let Some(enabled) = p.enabled {
                saved.enabled = enabled;
                // That was the owner's answer to it.
                if enabled
                    && saved.detail.as_deref()
                        == Some(settings_lost_text(&conn.def, conn.place).as_str())
                {
                    saved.detail = None;
                    saved.detail_at = None;
                }
            }
            if let Some(read_only) = p.read_only {
                saved.read_only = read_only;
            }
            if let Some(denied) = p.denied_tools {
                saved.denied_tools = denied;
            }
            if let Some(auto_allow) = p.auto_allow {
                saved.auto_allow = auto_allow;
            }
            before
        };
        // A choice of the owner's that won't last isn't made: a denied tool
        // mustn't come back with the next restart, unsaid.
        if let Err(e) = self.persist() {
            *lock(&conn.saved) = before;
            return Err(failed(format!(
                "Can't keep {}'s settings, so nothing changed: {e}",
                conn.def.title
            )));
        }
        // Another endpoint: its sessions and its tools aren't these.
        if lock(&conn.saved).read_only != before.read_only {
            self.forget_sessions(&p.service);
            *lock(&conn.catalog_read) = None;
        }
        (self.deps.changed)(&self.info(conn));
        Ok(self.info(conn))
    }

    // ---- Tokens.

    async fn load_secret(&self, conn: &Conn) -> io::Result<Option<Secret>> {
        if conn.loaded.load(Ordering::Relaxed) {
            return Ok(lock(&conn.secret).clone());
        }
        let store = self.store.clone();
        let name = conn.def.name.clone();
        // Without the refresh lock, so a slow store can't hold up a sign-in. What
        // comes back may be out of date by then: a login held since is newer
        // than anything read from before it, whatever this read found.
        let read = tokio::task::spawn_blocking(move || store.load(&name))
            .await
            .map_err(io::Error::other)?;
        let secret = {
            let mut held = lock(&conn.secret);
            if conn.loaded.load(Ordering::Relaxed) {
                return Ok(held.clone());
            }
            let secret = read?;
            *held = secret.clone();
            conn.loaded.store(true, Ordering::Relaxed);
            secret
        };
        // Whatever the state file says, the login's the truth of whether
        // there is one: as it is now, should a sign-in have come since.
        self.update(conn, |saved| match &*lock(&conn.secret) {
            None => {
                saved.state = ServiceState::Disconnected;
            }
            Some(s) => {
                // Only the owner ever has one of these.
                if s.refresh.is_some() {
                    saved.role = ServiceRole::Owner;
                }
                if saved.state == ServiceState::Disconnected {
                    saved.state = ServiceState::Connected;
                }
            }
        });
        Ok(secret)
    }

    /// Holds `secret` as the login now, the one before it being returned. With
    /// `loaded` set under the same lock, so a read that began earlier can't land
    /// on top of it.
    fn hold_login(&self, conn: &Conn, secret: Option<Secret>) -> Option<Secret> {
        let mut held = lock(&conn.secret);
        conn.loaded.store(true, Ordering::Relaxed);
        std::mem::replace(&mut *held, secret)
    }

    /// Writes a login to the store, durably, on a thread that may block.
    async fn write_secret(&self, conn: &Conn, secret: Secret) -> io::Result<()> {
        let store = self.store.clone();
        let name = conn.def.name.clone();
        tokio::task::spawn_blocking(move || store.store(&name, &secret))
            .await
            .map_err(io::Error::other)?
    }

    /// A token to call with: the current one, or a refreshed one if it's
    /// running out or `rejected` is what's current. One refresh at a time:
    /// whoever waits gets what it made.
    ///
    /// On a task of its own, which callers only wait on: a call's task ends when
    /// its shim goes, and a refresh cut short there would lose a token the
    /// service has already rotated.
    async fn access(&self, conn: &Conn, rejected: Option<&str>) -> Result<String, AuthFailure> {
        let unavailable =
            |why: &str| AuthFailure::Unavailable(format!("this broker is stopping ({why})"));
        let me = self.me.upgrade().ok_or_else(|| unavailable("it's gone"))?;
        let conn = self
            .conn(&conn.def.name)
            .cloned()
            .ok_or(AuthFailure::NotConnected)?;
        let rejected = rejected.map(str::to_owned);
        tokio::spawn(async move { me.access_now(&conn, rejected.as_deref()).await })
            .await
            .map_err(|e| unavailable(&e.to_string()))?
    }

    async fn access_now(&self, conn: &Conn, rejected: Option<&str>) -> Result<String, AuthFailure> {
        // Loaded first, as what's in the store can make this host the owner.
        self.load_secret(conn).await.map_err(|e| {
            AuthFailure::Unavailable(format!("{} can't be read ({e})", conn.place.store()))
        })?;
        if lock(&conn.saved).role == ServiceRole::Follower {
            return self.access_as_follower(conn, rejected).await;
        }
        let _one = conn.refreshing.lock().await;
        let secret = self
            .load_secret(conn)
            .await
            .map_err(|e| {
                AuthFailure::Unavailable(format!("{} can't be read ({e})", conn.place.store()))
            })?
            .ok_or(AuthFailure::NotConnected)?;
        let state = lock(&conn.saved).state;
        if matches!(state, ServiceState::NeedsReconnect | ServiceState::Revoked) {
            return Err(AuthFailure::NeedsReconnect);
        }
        let turned_down = rejected == Some(secret.access.as_str());
        let running_out = secret
            .expires_at
            .is_some_and(|at| at - REFRESH_EARLY <= (self.deps.clock)());
        // Refreshed already, by whoever held this lock before.
        if !turned_down && (rejected.is_some() || !running_out) {
            return Ok(secret.access);
        }
        self.refresh(conn, secret).await
    }

    /// A token as a follower: the one it was given if that's still good, or else
    /// it says it wants another and waits a moment to be given it, by way of the
    /// Mac. It never refreshes: it has nothing to do so with.
    async fn access_as_follower(
        &self,
        conn: &Conn,
        rejected: Option<&str>,
    ) -> Result<String, AuthFailure> {
        if let Some(token) = rejected {
            lock(&conn.following).rejected = Some(token.to_owned());
        }
        if let Some(answer) = self.follower_answer(conn) {
            return answer;
        }
        self.update(conn, |s| s.wants_token = true);
        let waited_lately = lock(&conn.following)
            .gave_up
            .is_some_and(|at| at.elapsed() < ASK_AGAIN_AFTER);
        if !waited_lately {
            let deadline = Instant::now() + self.deps.token_wait;
            while Instant::now() < deadline {
                tokio::time::sleep(TOKEN_LOOK).await;
                if let Some(answer) = self.follower_answer(conn) {
                    return answer;
                }
            }
            lock(&conn.following).gave_up = Some(Instant::now());
        }
        self.note(
            conn,
            ServiceState::OwnerUnreachable,
            Some(format!(
                "The access token here is no good and {} didn't give another",
                lock(&conn.saved)
                    .owner
                    .clone()
                    .unwrap_or_else(|| "the host that owns it".to_owned())
            )),
        );
        Err(AuthFailure::OwnerUnreachable)
    }

    /// What a follower can say now, if it needn't wait: `None` if it's to ask.
    fn follower_answer(&self, conn: &Conn) -> Option<Result<String, AuthFailure>> {
        let (state, owner) = {
            let saved = lock(&conn.saved);
            (saved.state, saved.owner.clone())
        };
        if matches!(
            state,
            ServiceState::NeedsReconnect | ServiceState::Revoked
        ) {
            return Some(Err(AuthFailure::NeedsReconnect));
        }
        let held = lock(&conn.secret).clone();
        let Some(held) = held else {
            return owner.is_none().then_some(Err(AuthFailure::NotConnected));
        };
        let refused = lock(&conn.following).rejected.as_deref() == Some(held.access.as_str());
        let run_out = held.expires_at.is_some_and(|at| at <= (self.deps.clock)());
        (!refused && !run_out).then_some(Ok(held.access))
    }

    /// Refreshes any login this host owns that's past half its life, so followers
    /// are handed a token with the most time on it, to carry them through the
    /// owner or the Mac being away. Failing is fine: the next pass tries again,
    /// with the current token good for hours yet.
    pub async fn refresh_due(&self) {
        for conn in &self.conns {
            if self.load_secret(conn).await.is_err() {
                continue;
            }
            let _one = conn.refreshing.lock().await;
            let (role, state) = {
                let saved = lock(&conn.saved);
                (saved.role, saved.state)
            };
            if role != ServiceRole::Owner
                || matches!(state, ServiceState::NeedsReconnect | ServiceState::Revoked)
            {
                continue;
            }
            // As it is now that the lock's held: a call may just have refreshed it.
            let Some(secret) = lock(&conn.secret).clone() else {
                continue;
            };
            let (Some(from), Some(to)) = (secret.obtained_at, secret.expires_at) else {
                continue;
            };
            if (self.deps.clock)() >= from + (to - from) / 2 {
                let _ = self.refresh(conn, secret).await;
            }
        }
    }

    /// The owner's current access token, for its followers. Never the refresh
    /// token. `rejected` is a token a follower had turned down: if it's still the
    /// current one, this refreshes first.
    pub async fn token(
        &self,
        service: &str,
        rejected: Option<&str>,
    ) -> Result<ServiceAccess, ErrorObject> {
        let conn = self.require(service)?;
        let held = self.load_secret(conn).await.ok().flatten();
        let answer = |state, access: Option<String>, expires_at| ServiceAccess {
            service: service.to_owned(),
            state,
            access,
            expires_at,
            detail: lock(&conn.saved).detail.clone(),
        };
        if lock(&conn.saved).role != ServiceRole::Owner {
            return if held.is_none() {
                Ok(answer(ServiceState::Disconnected, None, None))
            } else {
                Err(ErrorObject::new(
                    ErrorCode::Conflict,
                    format!("This host doesn't own {}'s login", conn.def.title),
                ))
            };
        }
        match self.access(conn, rejected).await {
            Ok(access) => {
                let expires_at = lock(&conn.secret).as_ref().and_then(|s| s.expires_at);
                Ok(answer(ServiceState::Connected, Some(access), expires_at))
            }
            Err(AuthFailure::NotConnected) => Ok(answer(ServiceState::Disconnected, None, None)),
            Err(AuthFailure::NeedsReconnect | AuthFailure::OwnerUnreachable) => {
                Ok(answer(lock(&conn.saved).state, None, None))
            }
            Err(AuthFailure::Unavailable(why)) => Err(failed(why)),
        }
    }

    /// Makes this host a follower of a login another host owns, using the token
    /// given. If it held a login of its own, that's forgotten first, here and at
    /// the service, so there's never a second host with a refresh token.
    pub async fn follow(
        self: &Arc<Self>,
        p: ServiceFollowParams,
    ) -> Result<ServiceInfo, ErrorObject> {
        let conn = self.require(&p.service)?.clone();
        let refreshing = conn.refreshing.lock().await;
        let _ = self.load_secret(&conn).await;
        let old = lock(&conn.secret).clone();
        let now = (self.deps.clock)();
        let next = p.access.clone().map(|access| Secret {
            access,
            refresh: None,
            obtained_at: Some(now),
            expires_at: p.expires_at,
            client_id: None,
            token_endpoint: None,
            revocation_endpoint: None,
            scopes: Vec::new(),
        });
        let store = self.store.clone();
        let name = conn.def.name.clone();
        let written = {
            let next = next.clone();
            tokio::task::spawn_blocking(move || match &next {
                Some(secret) => store.store(&name, secret),
                None => store.delete(&name),
            })
            .await
            .map_err(io::Error::other)
            .and_then(|r| r)
        };
        // Used whether or not it could be kept: this only saves asking again
        // after a restart.
        self.hold_login(&conn, next);
        drop(refreshing);
        let new_owner = lock(&conn.saved).owner != p.owner;
        let was_owner = old.as_ref().is_some_and(|s| s.refresh.is_some());
        if new_owner || was_owner || p.access.is_none() {
            self.forget_sessions(&p.service);
            *lock(&conn.catalog_read) = None;
        }
        let refused_again = {
            let mut following = lock(&conn.following);
            following.gave_up = None;
            let refused_again = p.access.is_some() && following.rejected == p.access;
            if !refused_again {
                following.rejected = None;
            }
            refused_again
        };
        let state = match p.state {
            // Which is only how the owner happens to be just now.
            ServiceState::Refreshing | ServiceState::OwnerUnreachable => ServiceState::Connected,
            other => other,
        };
        let at = p.detail.as_ref().map(|_| now);
        self.update(&conn, |s| {
            s.role = ServiceRole::Follower;
            s.owner = p.owner.clone();
            s.state = state;
            s.wants_token = refused_again;
            s.detail = p.detail.clone();
            s.detail_at = at;
            if state == ServiceState::Disconnected {
                s.tools.clear();
                s.instructions = None;
            }
        });
        if let Some(old) = old.filter(|s| s.refresh.is_some())
            && let Some(endpoint) = &old.revocation_endpoint
        {
            let token = old.refresh.as_deref().unwrap_or(&old.access);
            oauth::revoke(&self.http, endpoint, token, old.client_id.as_deref()).await;
        }
        written.map_err(|e| {
            failed(format!(
                "Can't keep the access token for {}: {e}",
                conn.def.title
            ))
        })?;
        // Agents see its tools soon: they ask every few minutes.
        if state == ServiceState::Connected && lock(&conn.saved).tools.is_empty() {
            let (services, reading) = (self.clone(), conn.clone());
            tokio::spawn(async move {
                let _ = services.catalog(&reading).await;
            });
        }
        Ok(self.info(&conn))
    }

    /// The services this broker knows, by name.
    pub fn names(&self) -> Vec<String> {
        self.conns.iter().map(|c| c.def.name.clone()).collect()
    }

    /// How `service` stands on this host.
    pub fn info_of(&self, service: &str) -> Option<ServiceInfo> {
        self.conn(service).map(|c| self.info(c))
    }

    /// Where the Mac has `service`'s login, and whether it disconnected it.
    pub fn assignment(&self, service: &str) -> Option<(Option<String>, bool)> {
        self.conn(service).map(|c| {
            let saved = lock(&c.saved);
            (saved.assigned.clone(), saved.released)
        })
    }

    /// The Mac records that `host` holds `service`'s login now.
    pub fn assign(&self, service: &str, host: &str) {
        if let Some(conn) = self.conn(service) {
            self.update(conn, |s| {
                s.assigned = Some(host.to_owned());
                s.released = false;
            });
        }
    }

    /// The Mac records that `service` was disconnected. Any host still saying it
    /// owns it is then told otherwise, and not believed.
    pub fn release(&self, service: &str) {
        if let Some(conn) = self.conn(service) {
            self.update(conn, |s| {
                s.assigned = None;
                s.released = true;
            });
        }
    }

    async fn refresh(&self, conn: &Conn, secret: Secret) -> Result<String, AuthFailure> {
        let title = &conn.def.title;
        let (Some(refresh), Some(endpoint), Some(client_id)) = (
            secret.refresh.as_deref(),
            secret.token_endpoint.as_deref(),
            secret.client_id.as_deref(),
        ) else {
            self.note(
                conn,
                ServiceState::NeedsReconnect,
                Some(format!("{title} gave no way to refresh its login")),
            );
            return Err(AuthFailure::NeedsReconnect);
        };
        self.note(conn, ServiceState::Refreshing, None);
        let tokens = oauth::token(
            &self.http,
            endpoint,
            &[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", client_id),
                ("resource", &conn.def.endpoint),
            ],
        )
        .await;
        let tokens = match tokens {
            Ok(t) => t,
            Err(TokenError::Refused { code, detail }) => {
                if code == "invalid_client" {
                    self.update(conn, |s| s.client_stale = true);
                }
                let tail = if detail.is_empty() {
                    String::new()
                } else {
                    format!(": {detail}")
                };
                self.note(
                    conn,
                    ServiceState::NeedsReconnect,
                    Some(format!("{title} refused the refresh ({code}){tail}")),
                );
                return Err(AuthFailure::NeedsReconnect);
            }
            Err(TokenError::Unavailable(why)) => {
                let why = format!("can't refresh ({why})");
                self.note(conn, ServiceState::Connected, Some(why.clone()));
                return Err(AuthFailure::Unavailable(self.scrub(conn, &why)));
            }
        };
        let now = (self.deps.clock)();
        let rotated = Secret {
            access: tokens.access.clone(),
            // A service that doesn't rotate keeps the one it has.
            refresh: tokens.refresh.or_else(|| secret.refresh.clone()),
            obtained_at: Some(now),
            expires_at: tokens.expires_in.map(|s| now + s),
            ..secret
        };
        // Write, then use. If the write fails, or a crash comes first, the new
        // token is never used, and the next attempt replays the old refresh
        // token, which a service that rotates takes within its replay grace.
        if let Err(e) = self.write_secret(conn, rotated.clone()).await {
            let why = format!("can't keep the refreshed login ({e})");
            self.note(conn, ServiceState::Connected, Some(why.clone()));
            return Err(AuthFailure::Unavailable(why));
        }
        self.hold_login(conn, Some(rotated));
        self.update(conn, |s| {
            s.state = ServiceState::Connected;
            s.detail = None;
            s.detail_at = None;
        });
        Ok(tokens.access)
    }

    // ---- Upstream.

    fn forget_sessions(&self, service: &str) {
        lock(&self.sessions).retain(|(_, s), _| s != service);
    }

    fn session_cell(&self, agent: &str, service: &str) -> Arc<SessionCell> {
        let mut sessions = lock(&self.sessions);
        sessions.retain(|_, c| lock(&c.used).is_none_or(|t| t.elapsed() < SESSION_IDLE));
        let cell = sessions
            .entry((agent.to_owned(), service.to_owned()))
            .or_default()
            .clone();
        *lock(&cell.used) = Some(Instant::now());
        cell
    }

    async fn post(
        &self,
        endpoint: &str,
        token: &str,
        session: Option<&UpstreamSession>,
        body: &Value,
    ) -> Result<Reply, String> {
        let bearer = format!("Bearer {token}");
        let mut headers = vec![
            ("Authorization", bearer.as_str()),
            ("Accept", "application/json, text/event-stream"),
        ];
        if let Some(s) = session {
            if let Some(id) = &s.id {
                headers.push(("Mcp-Session-Id", id));
            }
            headers.push(("MCP-Protocol-Version", &s.protocol));
        }
        self.http
            .send(
                Method::POST,
                endpoint,
                &headers,
                Some(("application/json", body.to_string().into_bytes())),
                CALL_TIMEOUT,
            )
            .await
    }

    /// Opens a session with the service.
    async fn initialize(&self, conn: &Conn, token: &str) -> Result<UpstreamSession, Rejected> {
        let endpoint = conn.endpoint();
        let init = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "VibeOS", "version": env!("CARGO_PKG_VERSION")},
            },
        });
        let reply = self
            .post(&endpoint, token, None, &init)
            .await
            .map_err(Rejected::Other)?;
        if reply.status == 401 {
            return Err(Rejected::Token);
        }
        if reply.status != 200 {
            return Err(Rejected::Other(format!("it answered {}", reply.status)));
        }
        let id = reply.header("mcp-session-id").map(str::to_owned);
        let (sink, messages) = mpsc::channel::<Value>(16);
        let request = json!(0);
        let (read, answer) = tokio::join!(
            read_messages(reply, Some(&request), sink),
            answer_to(messages, &request)
        );
        read.map_err(Rejected::Other)?;
        let answer = answer.ok_or_else(|| Rejected::Other("it didn't answer".into()))?;
        let protocol = answer
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .ok_or_else(|| Rejected::Other("it didn't agree a protocol".into()))?
            .to_owned();
        // What the service tells its clients, for the shim to tell its own.
        let instructions = answer
            .pointer("/result/instructions")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if instructions.is_some() {
            self.update(conn, |s| s.instructions = instructions);
        }
        let session = UpstreamSession { id, protocol };
        let ready = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let reply = self
            .post(&endpoint, token, Some(&session), &ready)
            .await
            .map_err(Rejected::Other)?;
        if reply.status == 401 {
            return Err(Rejected::Token);
        }
        Ok(session)
    }

    /// Sends `message` to the service for `agent`, and gives `sink` everything
    /// that comes back, as it comes. It handles a token turned down, once, by
    /// refreshing, and a session forgotten, once, by making a new one: neither
    /// reached the tool, so retrying a call is safe.
    async fn exchange(
        &self,
        conn: &Conn,
        agent: &str,
        message: &Value,
        sink: mpsc::Sender<Value>,
    ) -> Result<(), CallError> {
        let request_id = message.get("method").and(message.get("id")).cloned();
        let mut rejected: Option<String> = None;
        let mut session_retried = false;
        let cell = self.session_cell(agent, &conn.def.name);
        for _ in 0..4 {
            let token = self
                .access(conn, rejected.as_deref())
                .await
                .map_err(CallError::Auth)?;
            // A second refusal in a row is of a token just refreshed.
            let refused_before = rejected.is_some();
            let session = {
                let mut held = cell.session.lock().await;
                if let Some(s) = &*held {
                    s.clone()
                } else {
                    // A client's answer or a notification has no session to
                    // go to: it's about nothing the service knows of.
                    if request_id.is_none() {
                        return Ok(());
                    }
                    match self.initialize(conn, &token).await {
                        Ok(s) => {
                            *held = Some(s.clone());
                            s
                        }
                        Err(Rejected::Token) => {
                            if refused_before {
                                match self.turned_down(conn, &token) {
                                    Some(e) => return Err(e),
                                    None => {
                                        rejected = None;
                                        continue;
                                    }
                                }
                            }
                            rejected = Some(token);
                            continue;
                        }
                        Err(Rejected::Other(why)) => {
                            return Err(CallError::Unavailable(why));
                        }
                    }
                }
            };
            let reply = self
                .post(&conn.endpoint(), &token, Some(&session), message)
                .await
                .map_err(CallError::Unavailable)?;
            match reply.status {
                401 => {
                    if refused_before {
                        match self.turned_down(conn, &token) {
                            Some(e) => return Err(e),
                            None => rejected = None,
                        }
                    } else {
                        rejected = Some(token);
                    }
                }
                404 if session.id.is_some() && !session_retried => {
                    session_retried = true;
                    *cell.session.lock().await = None;
                }
                200 => {
                    let answered = read_messages(reply, request_id.as_ref(), sink)
                        .await
                        .map_err(CallError::Unavailable)?;
                    return if answered || request_id.is_none() {
                        Ok(())
                    } else {
                        Err(CallError::Unavailable(
                            "it closed the stream without answering".into(),
                        ))
                    };
                }
                // Fine for what wants no answer. A request does, and without
                // this its client would wait for one for ever.
                202 | 204 if request_id.is_none() => return Ok(()),
                202 | 204 => {
                    return Err(CallError::Unavailable(
                        "it took the call but said it won't answer".into(),
                    ));
                }
                status => {
                    return Err(CallError::Unavailable(format!("it answered {status}")));
                }
            }
        }
        Err(CallError::Unavailable("it kept turning calls away".into()))
    }

    /// A token fresh from the service was turned down by it: the owner revoked
    /// access there. Unless it's no longer the login in use, as when a Reconnect
    /// came meanwhile: then it says nothing of that login, and `None` says to go
    /// again with the one there is now.
    fn turned_down(&self, conn: &Conn, token: &str) -> Option<CallError> {
        let current = lock(&conn.secret)
            .as_ref()
            .is_some_and(|s| s.access == token);
        if !current {
            return None;
        }
        // Only the owner can tell a revoked login from a token that's merely
        // old. This host goes on asking, and the next call may have a good one.
        if lock(&conn.saved).role == ServiceRole::Follower {
            lock(&conn.following).rejected = Some(token.to_owned());
            self.update(conn, |s| s.wants_token = true);
            return Some(CallError::Auth(AuthFailure::OwnerUnreachable));
        }
        self.note(
            conn,
            ServiceState::Revoked,
            Some(format!(
                "{} turned down a freshly refreshed login: it was revoked there",
                conn.def.title
            )),
        );
        Some(CallError::Auth(AuthFailure::NeedsReconnect))
    }

    // ---- The catalog.

    /// The service's tools, as agents may see them. They come from what was
    /// last seen when the service can't say, so they still list with auth
    /// failing or the service away.
    async fn catalog(&self, conn: &Conn) -> Value {
        let _ = self.load_secret(conn).await;
        let (state, enabled) = {
            let saved = lock(&conn.saved);
            (saved.state, saved.enabled)
        };
        if state == ServiceState::Disconnected || !enabled {
            return json!({"tools": []});
        }
        let _one = conn.catalog_gate.lock().await;
        let fresh = lock(&conn.catalog_read).is_some_and(|t| t.elapsed() < CATALOG_FRESH);
        let healthy = matches!(state, ServiceState::Connected | ServiceState::Refreshing);
        if !fresh && healthy {
            self.read_catalog(conn).await;
        }
        let saved = lock(&conn.saved);
        let tools: Vec<&Value> = saved
            .tools
            .iter()
            .filter(|t| !conn.is_denied_in(&saved, t["name"].as_str().unwrap_or_default()))
            .collect();
        let mut result = json!({"tools": tools});
        if let Some(i) = &saved.instructions {
            result["instructions"] = json!(i);
        }
        result
    }

    async fn read_catalog(&self, conn: &Conn) {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        for page in 0..MAX_PAGES {
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let id = json!(format!("catalog-{page}"));
            let message =
                json!({"jsonrpc": "2.0", "id": id, "method": "tools/list", "params": params});
            let (sink, messages) = mpsc::channel::<Value>(16);
            let (went, answer) = tokio::join!(
                self.exchange(conn, CATALOG_AGENT, &message, sink),
                answer_to(messages, &id)
            );
            let Some(result) = went.ok().and(answer).and_then(|a| a.get("result").cloned()) else {
                return;
            };
            let Some(page_tools) = result.get("tools").and_then(Value::as_array) else {
                return;
            };
            tools.extend(page_tools.iter().cloned());
            cursor = result
                .get("nextCursor")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        *lock(&conn.catalog_read) = Some(Instant::now());
        self.update(conn, |s| s.tools = tools);
    }

    // ---- Shims.

    /// Gives a shim's conversation to the broker: messages sent on the
    /// returned channel are handled, and whatever comes back goes to `out`.
    /// Dropping the channel ends it, and any call still under way.
    pub fn open(
        self: &Arc<Self>,
        service: &str,
        agent: String,
        out: mpsc::Sender<Vec<u8>>,
    ) -> Result<mpsc::Sender<Vec<u8>>, String> {
        let conn = self
            .conn(service)
            .ok_or_else(|| format!("VibeOS has no shared service {service}"))?
            .clone();
        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
        let services = self.clone();
        tokio::spawn(async move {
            let mut calls = JoinSet::new();
            loop {
                tokio::select! {
                    message = rx.recv() => {
                        let Some(line) = message else { return };
                        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
                            continue;
                        };
                        // Room's kept past the limit for what isn't a new call: a
                        // cancel or an answer may be what frees one, and a refusal
                        // mustn't be lost. Beyond that the shim isn't reading what
                        // it's sent, so there's no one to tell.
                        if calls.len() >= MAX_IN_FLIGHT * 2 {
                            continue;
                        }
                        let (services, conn, agent, out) =
                            (services.clone(), conn.clone(), agent.clone(), out.clone());
                        if calls.len() >= MAX_IN_FLIGHT
                            && message.get("method").is_some()
                            && let Some(id) = message.get("id").cloned()
                        {
                            let error = json!({"code": -32000, "message": format!(
                                "Too many calls under way on this connection (at most {MAX_IN_FLIGHT}); try again shortly"
                            )});
                            // Off this loop: it must go on reading, to notice the
                            // shim going, however slowly that shim reads.
                            calls.spawn(async move {
                                send(&out, json!({"jsonrpc": "2.0", "id": id, "error": error})).await;
                            });
                            continue;
                        }
                        calls.spawn(async move {
                            services.handle(&conn, &agent, message, &out).await;
                        });
                    }
                    Some(_) = calls.join_next() => {}
                }
            }
        });
        Ok(tx)
    }

    /// Handles one message from a shim. It fails closed: only the two requests
    /// offered, a client's real answer and `notifications/*` go anywhere. All
    /// else is refused, as nothing else may reach the service with this host's
    /// token on it past the policy and audit below, batches and id-less calls
    /// included.
    async fn handle(&self, conn: &Conn, agent: &str, message: Value, out: &mpsc::Sender<Vec<u8>>) {
        let refuse = |code: i64, text: &str, id: Option<&Value>| {
            let error = json!({"code": code, "message": text});
            json!({"jsonrpc": "2.0", "id": id.unwrap_or(&Value::Null), "error": error})
        };
        if !message.is_object() {
            send(out, refuse(-32600, "Invalid Request", None)).await;
            return;
        }
        let id = message.get("id");
        match (message.get("method"), id) {
            (Some(Value::String(method)), Some(id)) => match method.as_str() {
                "tools/list" => {
                    let result = self.catalog(conn).await;
                    send(out, json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
                }
                "tools/call" => {
                    if let Some(answer) = self.call(conn, agent, id, &message, out).await {
                        send(out, answer).await;
                    }
                }
                other => {
                    let text = format!("Method not found: {other}");
                    send(out, refuse(-32601, &text, Some(id))).await;
                }
            },
            // A notification, which is all a request with no id may be.
            (Some(Value::String(method)), None) if method.starts_with("notifications/") => {
                self.forward(conn, agent, &message, out).await;
            }
            // A client's answer to something the service asked it.
            (None, Some(_))
                if message.get("result").is_some() || message.get("error").is_some() =>
            {
                self.forward(conn, agent, &message, out).await;
            }
            _ => send(out, refuse(-32600, "Invalid Request", id)).await,
        }
    }

    /// Sends the service something that wants no answer of its own.
    async fn forward(
        &self,
        conn: &Conn,
        agent: &str,
        message: &Value,
        out: &mpsc::Sender<Vec<u8>>,
    ) {
        if !lock(&conn.saved).enabled {
            return;
        }
        let (sink, mut messages) = mpsc::channel::<Value>(16);
        let passing = async {
            while let Some(m) = messages.recv().await {
                send(out, m).await;
            }
        };
        let _ = tokio::join!(self.exchange(conn, agent, message, sink), passing);
    }

    /// Makes one tool call: the answer to send if it hasn't been. Its audit
    /// line goes out before the answer does, and if the call's cut short the
    /// line still goes out, when the [`Audit`] is dropped.
    async fn call(
        &self,
        conn: &Conn,
        agent: &str,
        id: &Value,
        message: &Value,
        out: &mpsc::Sender<Vec<u8>>,
    ) -> Option<Value> {
        let name = message.pointer("/params/name").and_then(Value::as_str);
        let tool = name.unwrap_or_default();
        let mut audit = Audit {
            services: self,
            agent,
            service: &conn.def.name,
            tool,
            started: Instant::now(),
            done: false,
        };
        // A policy on names can't judge a call that doesn't have one.
        let Some(_) = name else {
            audit.finish("invalid");
            let error = json!({"code": -32602, "message": "A tool call needs the tool's name"});
            return Some(json!({"jsonrpc": "2.0", "id": id, "error": error}));
        };
        if !lock(&conn.saved).enabled {
            audit.finish("off");
            return Some(tool_error(id, &conn.off_text()));
        }
        if conn.is_denied(tool) {
            audit.finish("denied");
            return Some(tool_error(
                id,
                &format!(
                    "{}'s {tool} isn't allowed here (VibeOS → Settings → Services).",
                    conn.def.title
                ),
            ));
        }
        let (sink, mut messages) = mpsc::channel::<Value>(16);
        // In order, and waiting on a slow shim rather than dropping anything:
        // a stream of progress mustn't push out the answer.
        let forward = async {
            while let Some(m) = messages.recv().await {
                if m.get("id") == Some(id) && m.get("method").is_none() {
                    audit.finish(if m.pointer("/result/isError") == Some(&json!(true)) {
                        "tool_error"
                    } else {
                        "ok"
                    });
                }
                send(out, m).await;
            }
        };
        let (went, ()) = tokio::join!(self.exchange(conn, agent, message, sink), forward);
        match went {
            Ok(()) => None,
            Err(CallError::Auth(failure)) => {
                audit.finish(match failure {
                    AuthFailure::NotConnected => "not_connected",
                    AuthFailure::NeedsReconnect => "needs_reconnect",
                    AuthFailure::OwnerUnreachable => "owner_unreachable",
                    AuthFailure::Unavailable(_) => "unavailable",
                });
                Some(tool_error(id, &conn.failure_text(&failure)))
            }
            Err(CallError::Unavailable(why)) => {
                audit.finish("unavailable");
                let why = self.scrub(conn, &why);
                Some(tool_error(
                    id,
                    &conn.failure_text(&AuthFailure::Unavailable(why)),
                ))
            }
        }
    }
}

/// One tool call's audit line: agent, service, tool, outcome and time, and
/// never its arguments. Exactly one, however the call ends.
struct Audit<'a> {
    services: &'a Services,
    agent: &'a str,
    service: &'a str,
    tool: &'a str,
    started: Instant,
    done: bool,
}

impl Audit<'_> {
    fn finish(&mut self, outcome: &str) {
        if std::mem::replace(&mut self.done, true) {
            return;
        }
        self.services.record_call(ServiceAuditEntry {
            at: rfc3339((self.services.deps.clock)()),
            agent: printable(self.agent),
            service: self.service.to_owned(),
            tool: printable(self.tool),
            outcome: outcome.to_owned(),
            ms: self.started.elapsed().as_millis() as u64,
        });
    }
}

impl Drop for Audit<'_> {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
}

/// What's in the state file, and why it can't be used if it can't. A file that's
/// simply not there yet is no trouble: nothing's been chosen.
fn read_state(path: &std::path::Path) -> (HashMap<String, Saved>, Option<String>) {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (HashMap::new(), None),
        Err(e) => return (HashMap::new(), Some(e.to_string())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(all) => (all, None),
        Err(e) => (HashMap::new(), Some(e.to_string())),
    }
}

/// What a service says once its settings are lost. Turning it on clears this.
fn settings_lost_text(def: &Service, place: Place) -> String {
    let title = &def.title;
    let how = match place {
        Place::Mac => "in VibeOS → Settings → Services".to_owned(),
        Place::LinuxHost => format!(
            "with `vibe service on {}` here, or in VibeOS → Settings → Services on the Mac",
            def.name
        ),
    };
    format!(
        "This host's settings for {title} couldn't be read, so its read-only and denied tools \
         choices are lost. It's off until you turn it on {how}."
    )
}

/// Keeps a state file that can't be used, to look at, as the next write replaces
/// it. Beside it and not instead of it: if the file went and that write failed,
/// the next start would find no file, which means nothing's been chosen. Nothing
/// in it's secret.
fn set_aside(path: &std::path::Path) {
    let aside = path.with_extension("corrupt");
    let _ = fs::remove_file(&aside);
    let _ = fs::hard_link(path, &aside);
}

/// The last entries of the audit file.
fn read_audit(path: &std::path::Path) -> VecDeque<ServiceAuditEntry> {
    let mut kept = VecDeque::new();
    let Ok(file) = fs::File::open(path) else {
        return kept;
    };
    for line in io::BufReader::new(file).lines().map_while(Result::ok) {
        if let Ok(entry) = serde_json::from_str(&line) {
            if kept.len() >= AUDIT_KEPT {
                kept.pop_front();
            }
            kept.push_back(entry);
        }
    }
    kept
}

/// Adds an entry to the audit file, setting it aside once it's big. It's on this
/// host alone, as its calls are its own, and holds no argument or token.
fn append_audit(path: &std::path::Path, entry: &ServiceAuditEntry) -> io::Result<()> {
    if fs::metadata(path).is_ok_and(|m| m.len() > AUDIT_FILE_MAX) {
        fs::rename(path, path.with_extension("log.1"))?;
    }
    let mut line = serde_json::to_vec(entry).map_err(io::Error::other)?;
    line.push(b'\n');
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?
        .write_all(&line)
}

/// Gives a shim a message. It's gone if this fails, and that's all right.
async fn send(out: &mpsc::Sender<Vec<u8>>, message: Value) {
    let _ = out.send(message.to_string().into_bytes()).await;
}

/// Waits among messages for the answer to `id`.
async fn answer_to(mut messages: mpsc::Receiver<Value>, id: &Value) -> Option<Value> {
    let mut answer = None;
    while let Some(m) = messages.recv().await {
        if m.get("id") == Some(id) && m.get("method").is_none() {
            answer = Some(m);
        }
    }
    answer
}

/// Why a session couldn't be opened.
#[derive(Debug)]
enum Rejected {
    /// The service turned the token down.
    Token,
    Other(String),
}

fn invalid(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidParams, message)
}

/// `s` fit for one field of a log line.
fn printable(s: &str) -> String {
    s.chars()
        .take(128)
        .map(|c| {
            if c.is_ascii_graphic() && c != '=' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Reads an answer, JSON or an event stream, giving each message to `sink` as
/// it comes: nothing is held back until the end of a stream. Says whether the
/// answer to `request` was among them. Once it has been, a stream that breaks
/// loses nothing, so that's no error.
async fn read_messages(
    mut reply: Reply,
    request: Option<&Value>,
    sink: mpsc::Sender<Value>,
) -> Result<bool, String> {
    let mut answered = false;
    let mut give = async |m: Value| {
        if m.get("method").is_none() && m.get("id") == request {
            answered = true;
        }
        // Whoever was waiting has gone: reading on would be for nobody. Nor is
        // anything to come after the answer, whatever the stream is left doing,
        // and what's waiting on this mustn't wait on that.
        sink.send(m).await.is_ok() && !answered
    };
    let stream = reply
        .header("content-type")
        .is_some_and(|c| c.starts_with("text/event-stream"));
    if !stream {
        let body = reply.read(MAX_MESSAGE, CALL_TIMEOUT).await?;
        if body.iter().all(u8::is_ascii_whitespace) {
            return Ok(answered);
        }
        let v: Value =
            serde_json::from_slice(&body).map_err(|_| "an answer that isn't JSON".to_owned())?;
        let messages = match v {
            Value::Array(items) => items,
            v => vec![v],
        };
        for m in messages {
            if !give(m).await {
                break;
            }
        }
        return Ok(answered);
    }
    let mut events = EventStream::default();
    let mut failure = None;
    'reading: loop {
        match reply.next_chunk(CALL_TIMEOUT).await {
            Ok(None) => break,
            Ok(Some(chunk)) => match events.push(&chunk) {
                Ok(datas) => {
                    for data in datas {
                        if let Ok(v) = serde_json::from_str::<Value>(&data)
                            && !give(v).await
                        {
                            break 'reading;
                        }
                    }
                }
                Err(why) => {
                    failure = Some(why);
                    break;
                }
            },
            Err(why) => {
                failure = Some(why);
                break;
            }
        }
    }
    match failure {
        Some(why) if !answered => Err(why),
        _ => Ok(answered),
    }
}

/// Server-sent events, as far as a client of MCP needs them: each event's
/// `data`, whatever pieces it came in.
#[derive(Debug, Default)]
struct EventStream {
    line: Vec<u8>,
    data: String,
    has_data: bool,
}

impl EventStream {
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<String>, String> {
        let mut events = Vec::new();
        for &b in chunk {
            if b != b'\n' {
                self.line.push(b);
                if self.line.len() > MAX_MESSAGE {
                    return Err("an event longer than 16 MB".into());
                }
                continue;
            }
            let line = std::mem::take(&mut self.line);
            let line = line.strip_suffix(b"\r").unwrap_or(&line);
            if line.is_empty() {
                if self.has_data {
                    events.push(std::mem::take(&mut self.data));
                    self.has_data = false;
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix(b"data:") {
                let rest = rest.strip_prefix(b" ").unwrap_or(rest);
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(&String::from_utf8_lossy(rest));
                self.has_data = true;
                if self.data.len() > MAX_MESSAGE {
                    return Err("an event longer than 16 MB".into());
                }
            }
            // `event:`, `id:`, `retry:` and comments, which keep it alive.
        }
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_come_out_whatever_pieces_they_arrive_in() {
        let wire = b": keep-alive\n\nevent: message\ndata: {\"a\":1}\n\r\ndata: x\ndata: y\n\n";
        for size in 1..wire.len() {
            let mut stream = EventStream::default();
            let mut got = Vec::new();
            for piece in wire.chunks(size) {
                got.extend(stream.push(piece).unwrap());
            }
            assert_eq!(got, ["{\"a\":1}", "x\ny"], "in pieces of {size}");
        }
    }

    #[test]
    fn an_endless_event_is_refused() {
        let mut stream = EventStream::default();
        let huge = vec![b'x'; MAX_MESSAGE + 1];
        assert!(stream.push(&huge).is_err());
    }

    #[test]
    fn a_log_field_is_one_field() {
        assert_eq!(printable("list_issues"), "list_issues");
        assert_eq!(printable("a b\nc=d"), "a_b_c_d");
        assert_eq!(printable(&"x".repeat(500)).len(), 128);
    }
}
