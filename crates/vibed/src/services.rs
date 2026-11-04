//! Shared MCP services, from `vibed`'s side (PRD: Shared MCP services): how
//! agents are offered them, the socket their shims reach, and on a Mac the
//! relays it dials to its VMs.
//!
//! - **Offering.** Each launch gets the supported services' MCP servers, unless
//!   its profile has them off, with nothing written anywhere. What's offered
//!   doesn't depend on what's connected: tools appear on their own once it is.
//!   Only how they're offered differs by [`AgentKind`].
//! - **The local socket**, `run/.services.sock`. Every `vibed` serves it with
//!   its own broker, so a Linux host's agents need no Mac, and agents on a Mac
//!   use the Mac's.
//! - **One login.** Each service has one, held by one owning host. Signing in
//!   goes to that host, with the Mac only where the browser is. Links are a star,
//!   so the Mac carries the owner's access token to every other host
//!   ([`Core::give_tokens`]).
//! - **VMs.** A `linux` VM has a socket of its own, in `vibe-guest`. The Mac
//!   dials each awake one's vsock port with a relay to the Mac's broker; nothing
//!   listens for a guest to call in.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use serde_json::{Map, json};
use tokio::net::UnixStream;
use tokio::task::JoinHandle;
use vibe_core::model::{AgentKind, AgentState, LOCAL_HOST, ServiceRole, ServiceState};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, HostParams, ServiceAccess, ServiceAuditList, ServiceAuthorization,
    ServiceCompleteParams, ServiceFollowParams, ServiceHost, ServiceInfo, ServiceList,
    ServiceOverview, ServiceOverviewList, ServiceParams, ServiceSetParams, ServiceTokenParams,
    ServicesComplete, ServicesDisconnect, ServicesFollow, ServicesList, ServicesToken,
};
use vibe_core::store::{AgentRecord, StoreError};
use vibe_env::Backend;
use vibe_relay::local;
use vibe_services::relay;
use vibe_services::{Deps, Place, SecretStore, Service, Services};

use crate::core::{Core, error};
use crate::link::Link;

/// How often the Mac looks at how each service's followers stand, if nothing
/// wakes it sooner. A token running out is seen well ahead by the owner, and
/// anything sooner comes as an event.
const FOLLOW_EVERY: Duration = Duration::from_secs(30);

/// How long a host gets to answer anything about a service, so one that's stuck
/// can't hold up the others' tokens or the app.
const HOST_ANSWER: Duration = Duration::from_secs(3);

/// How often an owner looks for logins past half their life.
const REFRESH_LOOK: Duration = Duration::from_secs(60);

/// What the Mac last gave a host: who owns it, how it stands and the token.
type Given = (Option<String>, ServiceState, Option<String>);

/// What each host was last given, by (host, service).
pub(crate) type GivenTokens = HashMap<(String, String), Given>;

/// How often the Mac looks for VMs it should have a relay to.
const RELAY_EVERY: Duration = Duration::from_secs(1);

/// The first wait before dialing again after a relay fails, and the longest.
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// A relay that held this long was fine: the next starts without backoff.
const HELD: Duration = Duration::from_secs(30);

/// How the broker's made: `vibed`'s `main` picks the real services and this
/// machine's store, the Keychain on a Mac and a private file on a Linux host;
/// tests, a stand-in for each.
#[derive(Debug, Clone)]
pub struct ServicesConfig {
    pub services: Vec<Service>,
    pub store: Arc<dyn SecretStore>,
    pub place: Place,
    /// How long a follower's call waits to be given a token.
    pub token_wait: Duration,
}

impl Core {
    /// The broker, for [`Core::open`]. Its events go out as any other's.
    pub(crate) fn make_broker(
        config: ServicesConfig,
        state_file: std::path::PathBuf,
        clock: crate::core::Clock,
        entropy: crate::core::Entropy,
        changed: Arc<dyn Fn(&ServiceInfo) + Send + Sync>,
    ) -> Arc<Services> {
        let place = config.place;
        Services::new(
            config.services,
            config.store,
            state_file,
            Deps {
                clock,
                random: entropy,
                changed,
                audit: Arc::new(|line| crate::log!("vibed: {line}")),
                place,
                token_wait: config.token_wait,
            },
        )
    }

    fn broker(&self) -> Result<&Arc<Services>, ErrorObject> {
        self.services.as_ref().ok_or_else(|| {
            error(
                ErrorCode::CommandFailed,
                "This vibed runs no shared services broker",
            )
        })
    }

    pub fn services_list(&self, p: HostParams) -> Result<ServiceList, ErrorObject> {
        self.host(p.host.as_deref())?;
        Ok(self.broker()?.list())
    }

    pub async fn services_connect(
        &self,
        p: ServiceParams,
    ) -> Result<ServiceAuthorization, ErrorObject> {
        self.host(p.host.as_deref())?;
        self.broker()?.connect(&p.service).await
    }

    /// The second half of signing in, on the host that's to own the login, which
    /// makes the exchange itself. Whichever host it is owns the login from here
    /// on, and any other that did is told otherwise.
    pub async fn services_complete(
        &self,
        p: ServiceCompleteParams,
    ) -> Result<ServiceInfo, ErrorObject> {
        let broker = self.broker()?;
        // No tokens are given out until this is recorded: going by where the login
        // was, that would take it straight back off the new owner.
        let _settled = self.services_given.lock().await;
        let (host, info) = match p.host.as_deref().and_then(|h| self.link(h)) {
            Some(link) => (
                link.name.clone(),
                self.forward_typed::<ServicesComplete>(&link, &p).await?,
            ),
            None => {
                self.host(p.host.as_deref())?;
                let info = broker.complete(&p.service, &p.callback_url).await?;
                (LOCAL_HOST.to_owned(), info)
            }
        };
        broker.assign(&p.service, &host);
        self.services_wake.notify_one();
        Ok(info)
    }

    /// Forgets the one login, at its owner, wherever that is. If its owner can't
    /// be reached it's told when it next can be, and not believed until then.
    pub async fn services_disconnect(&self, p: ServiceParams) -> Result<ServiceInfo, ErrorObject> {
        let broker = self.broker()?;
        let (assigned, _) = broker.assignment(&p.service).ok_or_else(|| {
            error(
                ErrorCode::NotFound,
                format!("VibeOS has no shared service {}", p.service),
            )
        })?;
        let _settled = self.services_given.lock().await;
        match assigned.as_deref().and_then(|h| self.link(h)) {
            Some(link) => {
                let params = ServiceParams {
                    host: None,
                    service: p.service.clone(),
                };
                if let Err(e) =
                    answer_soon(self.forward_typed::<ServicesDisconnect>(&link, &params)).await
                {
                    crate::log!(
                        "vibed: can't disconnect {} on {} yet: {}",
                        p.service,
                        link.name,
                        e.message
                    );
                }
            }
            None => {
                self.host(p.host.as_deref())?;
                broker.disconnect(&p.service).await?;
            }
        }
        // Only for whoever gives out tokens: a Linux host has no one to tell.
        if !crate::core::lock(&self.links).is_empty() {
            broker.release(&p.service);
        }
        self.services_wake.notify_one();
        broker
            .info_of(&p.service)
            .ok_or_else(|| error(ErrorCode::NotFound, "no such service"))
    }

    pub async fn services_token(
        &self,
        p: ServiceTokenParams,
    ) -> Result<ServiceAccess, ErrorObject> {
        self.broker()?
            .token(&p.service, p.rejected.as_deref())
            .await
    }

    pub async fn services_follow(
        &self,
        p: ServiceFollowParams,
    ) -> Result<ServiceInfo, ErrorObject> {
        self.broker()?.follow(p).await
    }

    /// A host is being removed from this install, and goes on running by itself:
    /// it's to keep none of this install's logins or tokens, if it can be told.
    pub(crate) async fn services_host_leaving(&self, host: &str) {
        let (Some(broker), Some(link)) = (self.services.as_ref(), self.link(host)) else {
            return;
        };
        let _settled = self.services_given.lock().await;
        for service in broker.names() {
            let owner = broker.assignment(&service).and_then(|(a, _)| a);
            let told = if owner.as_deref() == Some(host) {
                broker.release(&service);
                let params = ServiceParams {
                    host: None,
                    service: service.clone(),
                };
                answer_soon(self.forward_typed::<ServicesDisconnect>(&link, &params)).await
            } else {
                let params = ServiceFollowParams {
                    service: service.clone(),
                    owner: None,
                    state: ServiceState::Disconnected,
                    access: None,
                    expires_at: None,
                    detail: None,
                };
                answer_soon(self.forward_typed::<ServicesFollow>(&link, &params)).await
            };
            if let Err(e) = told {
                crate::log!(
                    "vibed: can't tell {host} to forget {service}: {}",
                    e.message
                );
            }
        }
    }

    /// Each service's one connection, as the app shows it.
    pub async fn services_overview(&self) -> Result<ServiceOverviewList, ErrorObject> {
        let broker = self.broker()?;
        let links = self.up_links();
        let lists = self.service_lists(&links).await;
        let mut services = Vec::new();
        for name in broker.names() {
            let mine = broker
                .info_of(&name)
                .ok_or_else(|| error(ErrorCode::NotFound, "no such service"))?;
            let (assigned, _) = broker.assignment(&name).unwrap_or_default();
            let mut hosts = vec![ServiceHost {
                host: LOCAL_HOST.to_owned(),
                reachable: true,
                service: Some(mine.clone()),
            }];
            let mut names: Vec<String> = crate::core::lock(&self.links).keys().cloned().collect();
            names.sort();
            for host in names {
                let service = lists
                    .get(&host)
                    .and_then(|l| l.services.iter().find(|s| s.name == name))
                    .cloned();
                hosts.push(ServiceHost {
                    host,
                    reachable: service.is_some(),
                    service,
                });
            }
            let owner = assigned.unwrap_or_else(|| self.default_owner());
            let at_owner = hosts
                .iter()
                .find(|h| h.host == owner)
                .and_then(|h| h.service.clone());
            let holds = |s: &ServiceInfo| s.role == ServiceRole::Owner;
            let (state, signed_in, detail, detail_at) = match &at_owner {
                Some(s) => (s.state, holds(s), s.detail.clone(), s.detail_at.clone()),
                // Not known to hold it, and not there to ask: what's seen here
                // is what agents here have.
                None if broker.assignment(&name).is_some_and(|(a, _)| a.is_some()) => (
                    ServiceState::OwnerUnreachable,
                    true,
                    Some(format!("{owner} can't be reached from here")),
                    None,
                ),
                None => (ServiceState::Disconnected, false, None, None),
            };
            services.push(ServiceOverview {
                name: mine.name,
                title: mine.title,
                state,
                owner,
                signed_in,
                detail,
                detail_at,
                hosts,
            });
        }
        Ok(ServiceOverviewList { services })
    }

    /// Who holds a login that's yet to be made: a linked Linux host, as it's
    /// always on, the first by name that's up, or else just the first, as a link
    /// that's flapping is no reason to put it somewhere for good. With no linked
    /// host, this one.
    pub(crate) fn default_owner(&self) -> String {
        let links = crate::core::lock(&self.links);
        let mut hosts: Vec<(bool, &String)> =
            links.values().map(|l| (!l.connected(), &l.name)).collect();
        hosts.sort();
        hosts
            .first()
            .map_or_else(|| LOCAL_HOST.to_owned(), |(_, name)| (*name).clone())
    }

    fn up_links(&self) -> Vec<Arc<Link>> {
        crate::core::lock(&self.links)
            .values()
            .filter(|l| l.connected())
            .cloned()
            .collect()
    }

    /// What each of `links` says of its services, all at once, each given the same
    /// few seconds. A host that doesn't answer isn't in it.
    async fn service_lists(&self, links: &[Arc<Link>]) -> HashMap<String, ServiceList> {
        let params = HostParams::default();
        let mut lists = HashMap::new();
        // Links run only where this core knows itself, as an `Arc`.
        if let Some(core) = self.me.get().and_then(Weak::upgrade) {
            let mut asked = tokio::task::JoinSet::new();
            for link in links {
                let (core, link, params) = (Arc::clone(&core), Arc::clone(link), params.clone());
                asked.spawn(async move {
                    let said = answer_soon(core.forward_typed::<ServicesList>(&link, &params));
                    (link.name.clone(), said.await)
                });
            }
            for (host, said) in asked.join_all().await {
                if let Ok(list) = said {
                    lists.insert(host, list);
                }
            }
        } else {
            for link in links {
                let said = answer_soon(self.forward_typed::<ServicesList>(link, &params));
                if let Ok(list) = said.await {
                    lists.insert(link.name.clone(), list);
                }
            }
        }
        lists
    }

    /// One look at every service: the Mac gets the owner's current token and gives
    /// it to every other host it can reach. Links are a star, so this is how a
    /// token gets from one Linux host to another. With the Mac away nothing moves,
    /// and each follower has what it was last given until that runs out.
    pub(crate) async fn give_tokens(&self) {
        let Some(broker) = self.services.clone() else {
            return;
        };
        let links = self.up_links();
        let mut given = self.services_given.lock().await;
        // A link that went down may have missed anything: all's given again once
        // it's back.
        given.retain(|(host, _), _| host == LOCAL_HOST || links.iter().any(|l| &l.name == host));
        let lists = self.service_lists(&links).await;
        for service in broker.names() {
            self.give_token(&broker, &service, &links, &lists, &mut given)
                .await;
        }
    }

    async fn give_token(
        &self,
        broker: &Arc<Services>,
        service: &str,
        links: &[Arc<Link>],
        lists: &HashMap<String, ServiceList>,
        given: &mut GivenTokens,
    ) {
        let Some((mut assigned, mut released)) = broker.assignment(service) else {
            return;
        };
        let (no_links, owner_removed) = {
            let all = crate::core::lock(&self.links);
            let removed = assigned
                .as_deref()
                .is_some_and(|host| host != LOCAL_HOST && !all.contains_key(host));
            (all.is_empty(), removed)
        };
        // Its owner's gone from this install, and its login with it.
        if owner_removed {
            broker.release(service);
            assigned = None;
            released = true;
        }
        // A Linux host has no links: it's only ever told, by the Mac, and what it
        // was told stands. A Mac with none has nobody to give anything to, unless
        // it's just lost the host it was following.
        if no_links && !released {
            return;
        }
        let info_at = |host: &str| -> Option<ServiceInfo> {
            if host == LOCAL_HOST {
                return broker.info_of(service);
            }
            lists
                .get(host)?
                .services
                .iter()
                .find(|s| s.name == service)
                .cloned()
        };
        let mut targets: Vec<String> = links.iter().map(|l| l.name.clone()).collect();
        targets.push(LOCAL_HOST.to_owned());
        // This Mac has lost track, as after a reinstall or with its settings
        // gone. A host that says it holds the login does: there's no sense signing
        // in again. Unless this Mac disconnected it.
        if assigned.is_none() && !released {
            let owners: Vec<&String> = targets
                .iter()
                .filter(|h| {
                    info_at(h).is_some_and(|s| {
                        s.role == ServiceRole::Owner && s.state != ServiceState::Disconnected
                    })
                })
                .collect();
            let [owner] = owners[..] else {
                if owners.len() > 1 {
                    crate::log!("vibed: more than one host says it owns {service}: {owners:?}");
                }
                return;
            };
            broker.assign(service, owner);
            assigned = Some(owner.clone());
        }
        let access = match assigned.as_deref() {
            None => ServiceAccess {
                service: service.to_owned(),
                state: ServiceState::Disconnected,
                access: None,
                expires_at: None,
                detail: None,
            },
            Some(owner) => {
                let Some(first) = self.ask_owner(broker, owner, service, None, links).await else {
                    return;
                };
                // A follower that's had this very token turned down needs another,
                // so the owner is told it was, and refreshes.
                let refused = targets.iter().any(|host| {
                    host != owner
                        && info_at(host).is_some_and(|s| s.wants_token)
                        && given
                            .get(&(host.clone(), service.to_owned()))
                            .is_some_and(|g| g.2 == first.access)
                });
                match (&first.access, refused) {
                    (Some(token), true) => {
                        match self
                            .ask_owner(broker, owner, service, Some(token), links)
                            .await
                        {
                            Some(again) => again,
                            None => return,
                        }
                    }
                    _ => first,
                }
            }
        };
        for host in targets {
            if assigned.as_deref() == Some(host.as_str()) {
                continue;
            }
            let Some(there) = info_at(&host) else {
                continue;
            };
            let owner = assigned.as_deref().map(|o| {
                if o == LOCAL_HOST {
                    "the Mac".to_owned()
                } else {
                    o.to_owned()
                }
            });
            let key = (host.clone(), service.to_owned());
            let now_given: Given = (owner.clone(), access.state, access.access.clone());
            let settled = given.get(&key) == Some(&now_given)
                && there.role == ServiceRole::Follower
                && there.owner == owner
                && !there.wants_token;
            if settled {
                continue;
            }
            let params = ServiceFollowParams {
                service: service.to_owned(),
                owner,
                state: access.state,
                access: access.access.clone(),
                expires_at: access.expires_at,
                detail: access.detail.clone(),
            };
            let told = if host == LOCAL_HOST {
                broker.follow(params).await
            } else if let Some(link) = links.iter().find(|l| l.name == host) {
                answer_soon(self.forward_typed::<ServicesFollow>(link, &params)).await
            } else {
                continue;
            };
            match told {
                Ok(info) => {
                    // It's had that token turned down already, as this hadn't
                    // known. Now it does, so the owner can be told at once.
                    if info.wants_token && !given.contains_key(&key) {
                        self.services_wake.notify_one();
                    }
                    given.insert(key, now_given);
                }
                Err(e) => {
                    given.remove(&key);
                    crate::log!(
                        "vibed: can't give {host} {service}'s access token: {}",
                        e.message
                    );
                }
            }
        }
    }

    /// The token at `owner`, if it can say now.
    async fn ask_owner(
        &self,
        broker: &Arc<Services>,
        owner: &str,
        service: &str,
        rejected: Option<&str>,
        links: &[Arc<Link>],
    ) -> Option<ServiceAccess> {
        let said = if owner == LOCAL_HOST {
            broker.token(service, rejected).await
        } else {
            // Its link's down, so its followers go on with what they have.
            let link = links.iter().find(|l| l.name == owner)?;
            let params = ServiceTokenParams {
                service: service.to_owned(),
                rejected: rejected.map(str::to_owned),
            };
            answer_soon(self.forward_typed::<ServicesToken>(link, &params)).await
        };
        match said {
            Ok(access) => Some(access),
            Err(e) => {
                crate::log!(
                    "vibed: {owner} can't give {service}'s access token now: {}",
                    e.message
                );
                None
            }
        }
    }

    pub fn services_set(&self, p: ServiceSetParams) -> Result<ServiceInfo, ErrorObject> {
        self.host(p.host.as_deref())?;
        self.broker()?.set(p)
    }

    pub fn services_audit(&self, p: HostParams) -> Result<ServiceAuditList, ErrorObject> {
        self.host(p.host.as_deref())?;
        Ok(self.broker()?.audit())
    }

    /// Whether `record`'s agent gets the shared services at its next launch: what
    /// its profile says now, or if that's gone, what it said at the start.
    pub(crate) fn services_on(&self, record: &AgentRecord) -> bool {
        match self
            .store()
            .profile(&record.host, &record.project, &record.profile)
        {
            Ok(profile) => profile.shared_services,
            Err(_) => record.shared_services,
        }
    }

    /// What to offer `record`'s agent: the same JSON whichever kind it is, or
    /// nothing if its profile has them off or there's no broker here.
    pub fn shared_services(
        &self,
        backend: &Arc<dyn Backend>,
        record: &AgentRecord,
    ) -> Option<String> {
        (self.services.is_some() && self.services_on(record)).then(|| {
            let program = backend
                .services_program()
                .unwrap_or_else(|| self.vibe_cli.to_string_lossy().into_owned());
            servers_json(&program, &supported_names())
        })
    }

    /// The services the owner has chosen to run without Claude Code asking.
    pub(crate) fn services_auto_allowed(&self) -> Vec<String> {
        self.services
            .as_ref()
            .map(|b| b.auto_allowed())
            .unwrap_or_default()
    }

    /// The local socket, when the agent's on this machine rather than in a VM
    /// with a socket of its own.
    pub(crate) fn services_socket_env(
        &self,
        backend: &Arc<dyn Backend>,
    ) -> Option<(String, String)> {
        backend.services_program().is_none().then(|| {
            (
                local::SOCKET_ENV.to_owned(),
                self.services_socket().to_string_lossy().into_owned(),
            )
        })
    }

    pub fn services_socket(&self) -> std::path::PathBuf {
        self.layout.run_dir().join(local::HOST_SOCKET_NAME)
    }

    /// Whether a shim may be served: no agent of a profile with them off. It's
    /// not a wall, as what runs as this user can reach anything this user can,
    /// but an agent with them off doesn't get them by accident.
    fn admit_shim(&self, agent: Option<&str>) -> Result<(), String> {
        let Some(agent) = agent else {
            return Ok(());
        };
        let record = match self.store().agent(LOCAL_HOST, agent) {
            Ok(record) => record,
            // One this host doesn't know of isn't an agent of a profile.
            Err(StoreError::NotFound(_)) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        };
        if self.services_on(&record) {
            Ok(())
        } else {
            Err("shared services are off for this agent's profile".to_owned())
        }
    }

    /// Serves the local socket, and dials relays to any VMs, until the handles
    /// are aborted.
    pub fn spawn_services(self: &Arc<Self>) -> Vec<JoinHandle<()>> {
        let mut tasks = Vec::new();
        if self.services.is_none() {
            return tasks;
        }
        match crate::server::bind(&self.services_socket()) {
            Err(e) => crate::log!("vibed: can't serve shared services' socket: {e}"),
            Ok((listener, guard)) => match tokio::net::UnixListener::from_std(listener) {
                Err(e) => crate::log!("vibed: can't serve shared services' socket: {e}"),
                Ok(listener) => {
                    let core = Arc::downgrade(self);
                    tasks.push(tokio::spawn(async move {
                        // Held for as long as it's served.
                        let _guard = guard;
                        serve_socket(core, listener).await;
                    }));
                }
            },
        }
        let core = Arc::downgrade(self);
        tasks.push(tokio::spawn(async move { keep_relays(core).await }));
        // In the background, as a locked Keychain can take a while to say so.
        if let Some(broker) = self.services.clone() {
            tasks.push(tokio::spawn(async move {
                broker.load_logins().await;
                loop {
                    broker.refresh_due().await;
                    tokio::time::sleep(REFRESH_LOOK).await;
                }
            }));
        }
        let core = Arc::downgrade(self);
        let wake = Arc::clone(&self.services_wake);
        tasks.push(tokio::spawn(async move {
            keep_followers(core, wake).await;
        }));
        tasks
    }
}

/// Gives followers their tokens, whenever something changes, and every so often
/// in case that was missed.
async fn keep_followers(core: Weak<Core>, wake: Arc<tokio::sync::Notify>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(FOLLOW_EVERY) => {}
            () = wake.notified() => {}
        }
        let Some(core) = core.upgrade() else {
            return;
        };
        core.give_tokens().await;
    }
}

/// `call`, given a few seconds to answer.
async fn answer_soon<T>(
    call: impl Future<Output = Result<T, ErrorObject>>,
) -> Result<T, ErrorObject> {
    tokio::time::timeout(HOST_ANSWER, call)
        .await
        .unwrap_or_else(|_| {
            Err(error(
                ErrorCode::CommandFailed,
                format!("no answer in {} s", HOST_ANSWER.as_secs()),
            ))
        })
}

/// The supported services' names, which is what's offered.
pub fn supported_names() -> Vec<String> {
    vibe_services::supported()
        .into_iter()
        .map(|s| s.name)
        .collect()
}

/// The MCP servers for `services`, as JSON, each `program mcp-proxy <service>`
/// under its reserved name. It's Claude Code's `--mcp-config`, and what a Pi
/// extension reads from `VIBE_SERVICES`.
pub fn servers_json(program: &str, services: &[String]) -> String {
    let servers: Map<String, serde_json::Value> = services
        .iter()
        .map(|s| {
            (
                vibe_relay::server_name(s),
                json!({
                    "type": "stdio",
                    "command": program,
                    "args": vibe_relay::proxy_args(s),
                }),
            )
        })
        .collect();
    json!({"mcpServers": servers}).to_string()
}

/// What Pi's extension reads the servers from.
pub const SERVICES_ENV: &str = "VIBE_SERVICES";

/// How an agent's offered the shared services: what's added to its arguments
/// and its environment. Neither writes a file, and neither holds a secret.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Injection {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// How `kind` is offered `servers` (from [`servers_json`]). This is the only
/// place the shared services know about agent kinds.
///
/// - Claude Code takes them per launch with `--mcp-config`, which adds to the
///   owner's own servers rather than replacing them. No `--strict-mcp-config`,
///   and no allow rule unless the owner opted in, per service, so by default
///   Claude's permission prompts still govern writes.
/// - Pi, once #52 wires it, takes them through an extension that registers each
///   for the session: its backend adds `-e <that extension>`, and the extension
///   reads [`SERVICES_ENV`].
///
/// `auto_allowed` are the services the owner chose to run unasked. It's for
/// Claude Code alone: a Pi agent is never asked anyway.
pub fn injection(kind: AgentKind, servers: Option<&str>, auto_allowed: &[String]) -> Injection {
    let Some(servers) = servers else {
        return Injection::default();
    };
    match kind {
        AgentKind::Claude => {
            let mut args = vec!["--mcp-config".to_owned(), servers.to_owned()];
            if !auto_allowed.is_empty() {
                args.push("--allowedTools".to_owned());
                args.extend(
                    auto_allowed
                        .iter()
                        .map(|s| format!("mcp__{}__*", vibe_relay::server_name(s))),
                );
            }
            Injection {
                args,
                env: Vec::new(),
            }
        }
        AgentKind::Pi => Injection {
            args: Vec::new(),
            env: vec![(SERVICES_ENV.to_owned(), servers.to_owned())],
        },
    }
}

/// Accepts shims until the core goes.
async fn serve_socket(core: Weak<Core>, listener: tokio::net::UnixListener) {
    loop {
        let Ok((conn, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        };
        let Some(core) = core.upgrade() else {
            return;
        };
        let Some(broker) = core.services.clone() else {
            return;
        };
        let weak = Arc::downgrade(&core);
        let admit: relay::Admit = Arc::new(move |_, agent| match weak.upgrade() {
            Some(core) => core.admit_shim(agent),
            None => Err("vibed is stopping".to_owned()),
        });
        tokio::spawn(relay::serve_local(broker, conn, admit));
    }
}

/// One relay to a VM that the Mac keeps, or waits to dial again.
#[derive(Debug)]
struct Kept {
    task: Option<JoinHandle<io::Result<()>>>,
    started: Instant,
    backoff: Duration,
    next_try: Instant,
}

impl Kept {
    fn new() -> Kept {
        Kept {
            task: None,
            started: Instant::now(),
            backoff: MIN_BACKOFF,
            next_try: Instant::now(),
        }
    }

    /// Whether it should be dialed now. Notes one that's ended, and what it
    /// said if it's not the first time it said so.
    fn due(&mut self, what: &str) -> bool {
        if let Some(task) = &self.task {
            if !task.is_finished() {
                return false;
            }
            self.task = None;
            if self.started.elapsed() >= HELD {
                self.backoff = MIN_BACKOFF;
            }
            self.next_try = Instant::now() + self.backoff;
            self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
            crate::log!("vibed: the shared services relay to {what} ended");
        }
        Instant::now() >= self.next_try
    }

    fn failed(&mut self) {
        self.next_try = Instant::now() + self.backoff;
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
    }

    fn running(&mut self, task: JoinHandle<io::Result<()>>) {
        self.started = Instant::now();
        self.task = Some(task);
    }
}

impl Drop for Kept {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Keeps a relay to every awake VM whose agent has the services on. A VM's goes
/// when it sleeps, as `vibe-vm` exits, and the next look after it wakes dials
/// again; the shim inside never notices beyond a call or two saying the Mac's
/// away. Only a compartment that's a machine of its own has one, so on a Linux
/// host this finds nothing to do.
async fn keep_relays(core: Weak<Core>) {
    let mut vms: HashMap<String, Kept> = HashMap::new();
    loop {
        tokio::time::sleep(RELAY_EVERY).await;
        let Some(core) = core.upgrade() else {
            return;
        };
        let Some(broker) = core.services.clone() else {
            return;
        };
        keep_vm_relays(&core, &broker, &mut vms).await;
    }
}

async fn keep_vm_relays(core: &Core, broker: &Arc<Services>, vms: &mut HashMap<String, Kept>) {
    let agents = core.store().agents(LOCAL_HOST).unwrap_or_default();
    let wanted: Vec<&AgentRecord> = agents
        .iter()
        // A start included, so its agent has its tools from the first.
        .filter(|a| {
            !matches!(a.state, AgentState::Asleep | AgentState::Exited) && core.services_on(a)
        })
        .collect();
    vms.retain(|r, _| wanted.iter().any(|a| &a.agent_ref == r));
    for agent in wanted {
        let Some(backend) = core.backends.get(agent.environment) else {
            continue;
        };
        // Only a compartment with a machine of its own dials at all.
        if backend.services_program().is_none() {
            continue;
        }
        let Ok(handle) = Core::handle_of(agent) else {
            continue;
        };
        let kept = vms.entry(agent.agent_ref.clone()).or_insert_with(Kept::new);
        if !kept.due(&agent.agent_ref) {
            continue;
        }
        let dialed = tokio::task::spawn_blocking(move || backend.services_relay(&handle)).await;
        match dialed {
            Ok(Ok(Some(stream))) => match relay_over(stream) {
                Ok((read, write)) => {
                    kept.running(tokio::spawn(relay::serve_relay(
                        broker.clone(),
                        agent.agent_ref.clone(),
                        read,
                        write,
                    )));
                }
                Err(_) => kept.failed(),
            },
            // Not up yet, or not at all: only worth another look.
            _ => kept.failed(),
        }
    }
}

fn relay_over(
    stream: StdUnixStream,
) -> io::Result<(
    tokio::net::unix::OwnedReadHalf,
    tokio::net::unix::OwnedWriteHalf,
)> {
    stream.set_nonblocking(true)?;
    Ok(UnixStream::from_std(stream)?.into_split())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn linear() -> String {
        servers_json("/usr/local/bin/vibe", &["linear".to_owned()])
    }

    #[test]
    fn the_servers_are_a_stdio_shim_under_a_reserved_name() {
        let v: serde_json::Value = serde_json::from_str(&linear()).unwrap();
        assert_eq!(
            v,
            json!({"mcpServers": {"vibe-linear": {
                "type": "stdio",
                "command": "/usr/local/bin/vibe",
                "args": ["mcp-proxy", "linear"],
            }}})
        );
    }

    #[test]
    fn claude_gets_a_flag_and_pi_an_environment_variable_from_the_same_json() {
        let json = linear();
        assert_eq!(
            injection(AgentKind::Claude, Some(&json), &[]),
            Injection {
                args: vec!["--mcp-config".to_owned(), json.clone()],
                env: Vec::new(),
            }
        );
        assert_eq!(
            injection(AgentKind::Pi, Some(&json), &["linear".to_owned()]),
            Injection {
                args: Vec::new(),
                env: vec![("VIBE_SERVICES".to_owned(), json.clone())],
            }
        );
    }

    #[test]
    fn off_means_nothing_for_any_kind() {
        for kind in AgentKind::ALL {
            assert_eq!(
                injection(*kind, None, &["linear".to_owned()]),
                Injection::default()
            );
        }
    }

    #[test]
    fn linear_is_offered() {
        assert_eq!(supported_names(), ["linear"]);
    }

    #[test]
    fn claudes_own_servers_keep_loading() {
        // Nothing here may make Claude ignore the owner's servers.
        let i = injection(AgentKind::Claude, Some(&linear()), &[]);
        assert!(!i.args.iter().any(|a| a == "--strict-mcp-config"));
        assert!(!i.args.iter().any(|a| a.contains("allowedTools")));
    }

    #[test]
    fn a_service_runs_unasked_only_if_the_owner_opted_in() {
        let i = injection(AgentKind::Claude, Some(&linear()), &["linear".to_owned()]);
        assert_eq!(i.args[2..], ["--allowedTools", "mcp__vibe-linear__*"]);
    }
}
