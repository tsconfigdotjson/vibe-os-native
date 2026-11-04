//! The broker: its host's one holder of each service's login, and what every
//! call by that host's agents passes through. It knows nothing of which kind of
//! agent is calling: policy is on the service's own tool names, so it's the same
//! for all. Every `vibed` runs one for its own agents, and none needs another.
//!
//! **The lifecycle** (PRD: Shared MCP services). A service is `disconnected`,
//! `connected`, `refreshing`, `needs_reconnect` (a refresh was refused) or
//! `revoked` (a freshly refreshed token, or a key, was turned down). Refreshes
//! go one at a time per service, and a rotated token is persisted **before**
//! it's used: a crash between the two can't strand a used refresh token. When
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

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use http::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use vibe_core::model::{ServiceAuth, ServiceState};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, ServiceAuthorization, ServiceInfo, ServiceList, ServiceSetParams,
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

/// What this client offers a service.
const PROTOCOL: &str = "2025-06-18";

/// Whose upstream session the catalog is read in.
const CATALOG_AGENT: &str = "(catalog)";

/// How many pages of tools a catalog may have, should a service never stop.
const MAX_PAGES: usize = 50;

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
}

/// Where this broker runs, which is where its login gets mended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Mac,
    LinuxHost,
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
    #[serde(default = "disconnected")]
    state: ServiceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    auth: Option<ServiceAuth>,
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

impl Default for Saved {
    fn default() -> Self {
        Saved {
            enabled: true,
            read_only: false,
            denied_tools: Vec::new(),
            state: ServiceState::Disconnected,
            auth: None,
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
    /// Only the owner can fix it, on the Mac.
    NeedsReconnect,
    /// It may pass: the service or the Keychain can't be reached.
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

    fn needs_reconnect_text(&self) -> String {
        let title = &self.def.title;
        match self.place {
            Place::Mac => format!(
                "{title} needs reconnecting on the Mac (VibeOS → Settings → Services → Reconnect)."
            ),
            Place::LinuxHost => format!(
                "{title} needs reconnecting on this host (VibeOS → Settings → Services → Reconnect \
                 on the Mac, or `vibe service set-key {}` here).",
                self.def.name
            ),
        }
    }

    fn not_connected_text(&self) -> String {
        let title = &self.def.title;
        match self.place {
            Place::Mac => format!(
                "{title} isn't connected on the Mac (VibeOS → Settings → Services → Connect)."
            ),
            Place::LinuxHost => format!(
                "{title} isn't connected on this host (VibeOS → Settings → Services → Connect on \
                 the Mac, or `vibe service set-key {}` here).",
                self.def.name
            ),
        }
    }

    fn off_text(&self) -> String {
        let title = &self.def.title;
        match self.place {
            Place::Mac => {
                format!("{title} is turned off on the Mac (VibeOS → Settings → Services).")
            }
            Place::LinuxHost => {
                format!("{title} is turned off on this host (VibeOS → Settings → Services).")
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
    conns: Vec<Arc<Conn>>,
    http: Http,
    store: Arc<dyn SecretStore>,
    state_file: PathBuf,
    /// One write of the state file at a time.
    state_lock: Mutex<()>,
    deps: Deps,
    sessions: Mutex<HashMap<(String, String), Arc<SessionCell>>>,
    pending: Mutex<HashMap<String, Pending>>,
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
    /// Picks up from the state file and the store. Reads the store on this
    /// thread: it answers at once, and a locked Keychain is only noted.
    pub fn new(
        services: Vec<Service>,
        store: Arc<dyn SecretStore>,
        state_file: PathBuf,
        deps: Deps,
    ) -> Arc<Services> {
        let mut all: HashMap<String, Saved> = fs::read(&state_file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        let conns = services
            .into_iter()
            .map(|def| {
                let mut saved = all.remove(&def.name).unwrap_or_default();
                // A refresh is never under way as a daemon starts.
                if saved.state == ServiceState::Refreshing {
                    saved.state = ServiceState::Connected;
                }
                let (secret, loaded) = match store.load(&def.name) {
                    Ok(secret) => {
                        match (&secret, saved.state) {
                            (None, _) => {
                                saved.state = ServiceState::Disconnected;
                                saved.auth = None;
                            }
                            (Some(s), ServiceState::Disconnected) => {
                                saved.state = ServiceState::Connected;
                                saved.auth = Some(s.auth);
                            }
                            (Some(s), _) => saved.auth = Some(s.auth),
                        }
                        (secret, true)
                    }
                    Err(_) => (None, false),
                };
                Arc::new(Conn {
                    def,
                    place: deps.place,
                    secret: Mutex::new(secret),
                    loaded: AtomicBool::new(loaded),
                    refreshing: tokio::sync::Mutex::new(()),
                    saved: Mutex::new(saved),
                    catalog_read: Mutex::new(None),
                    catalog_gate: tokio::sync::Mutex::new(()),
                })
            })
            .collect();
        Arc::new(Services {
            conns,
            http: Http::new(),
            store,
            state_file,
            state_lock: Mutex::new(()),
            deps,
            sessions: Mutex::default(),
            pending: Mutex::default(),
        })
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
            auth: saved.auth,
            enabled: saved.enabled,
            read_only: saved.read_only,
            denied_tools: saved.denied_tools.clone(),
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

    /// Keeps the state file. Failing to is noted, not fatal: only settings and
    /// the last catalog are at stake, and the login's in the store.
    fn persist(&self) {
        let _one = lock(&self.state_lock);
        let all: HashMap<&str, Saved> = self
            .conns
            .iter()
            .map(|c| (c.def.name.as_str(), lock(&c.saved).clone()))
            .collect();
        let Ok(bytes) = serde_json::to_vec_pretty(&all) else {
            return;
        };
        let tmp = self.state_file.with_extension("new");
        let written = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .and_then(|mut f| io::Write::write_all(&mut f, &bytes))
            .and_then(|()| fs::rename(&tmp, &self.state_file));
        if let Err(e) = written {
            (self.deps.audit)(&format!(
                "services: can't keep {}: {e}",
                self.state_file.display()
            ));
        }
    }

    fn changed(&self, conn: &Conn) {
        self.persist();
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
        let mut text: String = text.chars().take(300).collect();
        if let Some(secret) = &*lock(&conn.secret) {
            text = text.replace(&secret.access, "[redacted]");
            if let Some(r) = &secret.refresh {
                text = text.replace(r, "[redacted]");
            }
        }
        text
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
        let stale = lock(&conn.saved).client_stale;
        let known = self
            .load_secret(conn)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.client_id);
        let client_id = match known.filter(|_| !stale) {
            Some(id) => id,
            None => oauth::register(&self.http, &md).await.map_err(|e| {
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
        if let Some(error) = get("error") {
            let said = get("error_description").unwrap_or(error);
            return Err(failed(format!(
                "{title} didn't sign you in: {}",
                said.chars().take(200).collect::<String>()
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
                return Err(failed(format!(
                    "{title} refused the sign-in ({code}){tail}; start again"
                )));
            }
            Err(TokenError::Unavailable(why)) => {
                return Err(failed(format!("Can't finish signing in to {title}: {why}")));
            }
        };
        let now = (self.deps.clock)();
        let secret = Secret {
            auth: ServiceAuth::Oauth,
            access: tokens.access,
            refresh: tokens.refresh,
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

    /// Connects with a personal API key, once the service has taken it.
    pub async fn connect_key(
        self: &Arc<Self>,
        service: &str,
        key: &str,
    ) -> Result<ServiceInfo, ErrorObject> {
        let conn = self.require(service)?;
        let key = key.trim();
        if key.is_empty() {
            return Err(invalid("that key is empty"));
        }
        let title = &conn.def.title;
        match self.initialize(conn, key).await {
            Ok(_) => {}
            Err(Rejected::Token) => {
                return Err(failed(format!("{title} refused that key")));
            }
            Err(Rejected::Other(why)) => {
                return Err(failed(format!("Can't check that key with {title}: {why}")));
            }
        }
        let secret = Secret {
            auth: ServiceAuth::ApiKey,
            access: key.to_owned(),
            refresh: None,
            expires_at: None,
            client_id: None,
            token_endpoint: None,
            revocation_endpoint: None,
            scopes: Vec::new(),
        };
        self.connected(conn, secret).await?;
        Ok(self.info(conn))
    }

    /// A new login: kept whole, replacing the old one, then in use.
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
        *lock(&conn.secret) = Some(secret.clone());
        conn.loaded.store(true, Ordering::Relaxed);
        drop(refreshing);
        self.forget_sessions(&conn.def.name);
        *lock(&conn.catalog_read) = None;
        self.update(conn, |s| {
            s.state = ServiceState::Connected;
            s.auth = Some(secret.auth);
            s.detail = None;
            s.detail_at = None;
            s.client_stale = false;
        });
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
        // As in `connected`: a refresh in flight mustn't write it back after.
        let refreshing = conn.refreshing.lock().await;
        tokio::task::spawn_blocking(move || store.delete(&name))
            .await
            .map_err(|e| failed(e.to_string()))?
            .map_err(|e| failed(format!("Can't forget the login: {e}")))?;
        let gone = lock(&conn.secret).take();
        conn.loaded.store(true, Ordering::Relaxed);
        drop(refreshing);
        self.forget_sessions(service);
        lock(&self.pending).remove(service);
        *lock(&conn.catalog_read) = None;
        self.update(conn, |s| {
            s.state = ServiceState::Disconnected;
            s.auth = None;
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
        let was_read_only = lock(&conn.saved).read_only;
        self.update(conn, |s| {
            if let Some(enabled) = p.enabled {
                s.enabled = enabled;
            }
            if let Some(read_only) = p.read_only {
                s.read_only = read_only;
            }
            if let Some(denied) = p.denied_tools {
                s.denied_tools = denied;
            }
        });
        // Another endpoint: its sessions and its tools aren't these.
        if lock(&conn.saved).read_only != was_read_only {
            self.forget_sessions(&p.service);
            *lock(&conn.catalog_read) = None;
        }
        Ok(self.info(conn))
    }

    // ---- Tokens.

    async fn load_secret(&self, conn: &Conn) -> io::Result<Option<Secret>> {
        if conn.loaded.load(Ordering::Relaxed) {
            return Ok(lock(&conn.secret).clone());
        }
        let store = self.store.clone();
        let name = conn.def.name.clone();
        let secret = tokio::task::spawn_blocking(move || store.load(&name))
            .await
            .map_err(io::Error::other)??;
        *lock(&conn.secret) = secret.clone();
        conn.loaded.store(true, Ordering::Relaxed);
        Ok(secret)
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
    async fn access(&self, conn: &Conn, rejected: Option<&str>) -> Result<String, AuthFailure> {
        let _one = conn.refreshing.lock().await;
        let secret = self
            .load_secret(conn)
            .await
            .map_err(|e| AuthFailure::Unavailable(format!("the Keychain can't be read ({e})")))?
            .ok_or(AuthFailure::NotConnected)?;
        let state = lock(&conn.saved).state;
        if matches!(state, ServiceState::NeedsReconnect | ServiceState::Revoked) {
            return Err(AuthFailure::NeedsReconnect);
        }
        let turned_down = rejected == Some(secret.access.as_str());
        if secret.auth == ServiceAuth::ApiKey {
            // There's no refreshing a key.
            if turned_down {
                self.note(
                    conn,
                    ServiceState::Revoked,
                    Some(format!("{} refused the API key", conn.def.title)),
                );
                return Err(AuthFailure::NeedsReconnect);
            }
            return Ok(secret.access);
        }
        let running_out = secret
            .expires_at
            .is_some_and(|at| at - REFRESH_EARLY <= (self.deps.clock)());
        // Refreshed already, by whoever held this lock before.
        if !turned_down && (rejected.is_some() || !running_out) {
            return Ok(secret.access);
        }
        self.refresh(conn, secret).await
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
            expires_at: tokens.expires_in.map(|s| now + s),
            ..secret
        };
        // Write, then use. If this fails or never returns, the new token is
        // never used, and the next attempt replays the old refresh token, which
        // a service that rotates takes within its replay grace.
        if let Err(e) = self.write_secret(conn, rotated.clone()).await {
            let why = format!("can't keep the refreshed login ({e})");
            self.note(conn, ServiceState::Connected, Some(why.clone()));
            return Err(AuthFailure::Unavailable(why));
        }
        *lock(&conn.secret) = Some(rotated);
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
                                return Err(self.turned_down(conn));
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
                        return Err(self.turned_down(conn));
                    }
                    rejected = Some(token);
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
                202 | 204 => return Ok(()),
                status => {
                    return Err(CallError::Unavailable(format!("it answered {status}")));
                }
            }
        }
        Err(CallError::Unavailable("it kept turning calls away".into()))
    }

    /// A token fresh from the service was turned down by it: the owner revoked
    /// access there.
    fn turned_down(&self, conn: &Conn) -> CallError {
        self.note(
            conn,
            ServiceState::Revoked,
            Some(format!(
                "{} turned down a freshly refreshed login: it was revoked there",
                conn.def.title
            )),
        );
        CallError::Auth(AuthFailure::NeedsReconnect)
    }

    // ---- The catalog.

    /// The service's tools, as agents may see them. They come from what was
    /// last seen when the service can't say, so they still list with auth
    /// failing or the service away.
    async fn catalog(&self, conn: &Conn) -> Value {
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
                        let (services, conn, agent, out) =
                            (services.clone(), conn.clone(), agent.clone(), out.clone());
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

    async fn handle(&self, conn: &Conn, agent: &str, message: Value, out: &mpsc::Sender<Vec<u8>>) {
        let id = message.get("id").cloned();
        match (message.get("method").and_then(Value::as_str), &id) {
            (Some("tools/list"), Some(id)) => {
                let result = self.catalog(conn).await;
                send(out, json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
            }
            (Some("tools/call"), Some(id)) => {
                if let Some(answer) = self.call(conn, agent, id, &message, out).await {
                    send(out, answer).await;
                }
            }
            (Some(method), Some(id)) => {
                let text = format!("Method not found: {method}");
                let error = json!({"code": -32601, "message": text});
                send(out, json!({"jsonrpc": "2.0", "id": id, "error": error})).await;
            }
            // A client's answer, or a notification such as a cancel.
            _ => {
                if lock(&conn.saved).enabled {
                    let (sink, mut messages) = mpsc::channel::<Value>(16);
                    let forward = async {
                        while let Some(m) = messages.recv().await {
                            send(out, m).await;
                        }
                    };
                    let _ = tokio::join!(self.exchange(conn, agent, &message, sink), forward);
                }
            }
        }
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
        let tool = message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut audit = Audit {
            write: &self.deps.audit,
            agent,
            service: &conn.def.name,
            tool,
            started: Instant::now(),
            done: false,
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
    write: &'a Arc<dyn Fn(&str) + Send + Sync>,
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
        (self.write)(&format!(
            "services: agent={} service={} tool={} outcome={outcome} ms={}",
            printable(self.agent),
            self.service,
            printable(self.tool),
            self.started.elapsed().as_millis()
        ));
    }
}

impl Drop for Audit<'_> {
    fn drop(&mut self) {
        self.finish("cancelled");
    }
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
        // Whoever was waiting has gone: reading on would be for nobody.
        sink.send(m).await.is_ok()
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
