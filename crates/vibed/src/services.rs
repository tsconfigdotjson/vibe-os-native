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
//!   use the Mac's. The Mac is only ever where the browser is: a host's login
//!   is that host's own (`services.*` take a `host`, and go over its link).
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
use vibe_core::model::{AgentKind, AgentState, LOCAL_HOST};
use vibe_core::rpc::{
    ErrorCode, ErrorObject, HostParams, ServiceAuditList, ServiceAuthorization,
    ServiceCompleteParams, ServiceInfo, ServiceKeyParams, ServiceList, ServiceParams,
    ServiceSetParams,
};
use vibe_core::store::{AgentRecord, StoreError};
use vibe_env::Backend;
use vibe_relay::local;
use vibe_services::relay;
use vibe_services::{Deps, Place, SecretStore, Service, Services};

use crate::core::{Core, error};

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

    pub async fn services_complete(
        &self,
        p: ServiceCompleteParams,
    ) -> Result<ServiceInfo, ErrorObject> {
        self.host(p.host.as_deref())?;
        self.broker()?.complete(&p.service, &p.callback_url).await
    }

    pub async fn services_connect_key(
        &self,
        p: ServiceKeyParams,
    ) -> Result<ServiceInfo, ErrorObject> {
        self.broker()?.connect_key(&p.service, &p.key).await
    }

    pub async fn services_disconnect(&self, p: ServiceParams) -> Result<ServiceInfo, ErrorObject> {
        self.host(p.host.as_deref())?;
        self.broker()?.disconnect(&p.service).await
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
            tasks.push(tokio::spawn(async move { broker.load_logins().await }));
        }
        tasks
    }
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
