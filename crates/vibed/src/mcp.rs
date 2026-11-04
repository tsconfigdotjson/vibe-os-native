//! MCP on this daemon's core (PRD: MCP interface). `vibe-mcp` is the
//! façade; this gives it the same handlers the socket API calls, so the two
//! can't drift apart.
//!
//! A call for a Linux host's agent goes to that host's `vibed` over its
//! link, as the socket's do; while the link is down, its agents are listed
//! as they were last seen, unavailable.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use vibe_core::limits::{self, HostLimits};
use vibe_core::model::EventKind;
use vibe_core::model::{LOCAL_HOST, MAC_HOST};
use vibe_core::rpc::{
    Agent, AgentCheckParams, AgentDeleteParams, AgentList, AgentMessagesParams, AgentOutputParams,
    AgentSent, AgentStartParams, AgentsCheck, AgentsDelete, AgentsList, AgentsMessages,
    AgentsOutput, AgentsSend, AgentsStart, ErrorCode, ErrorObject, HostList, HostParams, HostsList,
    McpSetParams, McpSettings, Method, Project, ProjectList, ProjectsList,
};
use vibe_mcp::http::Settings;
use vibe_mcp::{AgentView, Caller, Check, Daemon, Input, Message, Screen, Sent, Target};

use crate::core::{Core, error, invalid, limits_error, lock};
use crate::external::MacPeer;

use crate::hosts::call_timeout;

/// A value the Mac answered with, as `T`.
fn typed<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, ErrorObject> {
    serde_json::from_value(v).map_err(|e| {
        error(
            ErrorCode::InternalError,
            format!("Internal error: the Mac answered with something unexpected: {e}"),
        )
    })
}

/// Calls `M` on the Mac behind this host for `c`, whose key the Mac checks.
async fn mac_call<M: Method>(
    mac: &MacPeer,
    c: &Caller,
    params: &M::Params,
) -> Result<M::Result, ErrorObject> {
    let params = serde_json::to_value(params)
        .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?;
    let v = mac
        .call(c.key(), M::NAME, params, call_timeout(M::NAME))
        .await?;
    typed(v)
}

/// How often the MCP listeners are checked against `host.toml`.
pub const LISTEN_CHECK_EVERY: Duration = Duration::from_secs(5);

/// This machine's name, for the attach line from another machine.
fn machine_name() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(name) if ok && !name.is_empty() => name.to_owned(),
        _ => "this-host".to_owned(),
    }
}

/// The `vibe` an ssh command runs, by its path: over ssh, a Mac's shell
/// has sshd's `PATH`, without `/usr/local/bin`. Setup links it there
/// (PRD: Install and setup); otherwise it's the one beside `vibed`.
fn vibe_path() -> String {
    let linked = Path::new("/usr/local/bin/vibe");
    if linked.exists() {
        return linked.display().to_string();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.join("vibe")))
        .filter(|p| p.is_file() && !p.to_string_lossy().contains(char::is_whitespace))
        .map_or_else(|| "vibe".to_owned(), |p| p.display().to_string())
}

impl Core {
    /// The MCP port and whether it's served on Tailscale too, from
    /// `host.toml` as it is now, or `None` while it can't be read.
    pub fn mcp_settings(&self) -> Option<(u16, bool)> {
        limits::load(&self.layout.host_config())
            .ok()
            .map(|l| (l.mcp_port, l.mcp_tailscale))
    }

    /// The addresses MCP is served on with these settings: loopback, and
    /// with `tailscale` this machine's Tailscale addresses as they are now.
    fn mcp_addresses(&self, port: u16, tailscale: bool) -> Vec<SocketAddr> {
        let ts = if tailscale {
            (self.tailscale)()
        } else {
            Vec::new()
        };
        vibe_mcp::net::listen_on(port, ts)
    }

    fn mcp_info(&self, port: u16, tailscale: bool) -> McpSettings {
        let urls: Vec<String> = self
            .mcp_addresses(port, tailscale)
            .iter()
            .map(|a| format!("http://{a}{}", vibe_mcp::PATH))
            .collect();
        McpSettings {
            port,
            tailscale,
            url: urls[0].clone(),
            tailscale_urls: urls[1..].to_vec(),
        }
    }

    /// `mcp.get`: where MCP is served, as `host.toml` says now.
    pub fn mcp(&self, p: HostParams) -> Result<McpSettings, ErrorObject> {
        self.host(p.host.as_deref())?;
        let l = limits::load(&self.layout.host_config()).map_err(limits_error)?;
        Ok(self.mcp_info(l.mcp_port, l.mcp_tailscale))
    }

    /// `mcp.set`: writes the change into `host.toml`, keeping the rest of
    /// the file, and has the listeners follow it at once.
    pub fn set_mcp(&self, p: McpSetParams) -> Result<McpSettings, ErrorObject> {
        self.host(p.host.as_deref())?;
        let Some(on) = p.tailscale else {
            return Err(invalid("give at least one setting to change"));
        };
        let l = {
            let _one_at_a_time = lock(&self.limits_lock);
            limits::set_mcp_tailscale(&self.layout.host_config(), on).map_err(limits_error)?
        };
        crate::log!(
            "vibed: MCP on the Tailscale address turned {}",
            if on { "on" } else { "off" }
        );
        self.publish(self.event(EventKind::McpChanged));
        self.mcp_kick.notify_one();
        Ok(self.mcp_info(l.mcp_port, l.mcp_tailscale))
    }

    /// Serves MCP where `host.toml` says, until the task is aborted, and
    /// follows each change: at once after `mcp.set`, and within `every` of
    /// a hand edit. While the file can't be read, such as halfway through
    /// an edit, MCP stays where it was; before it ever could, it's on the
    /// default port.
    pub fn spawn_mcp(self: &Arc<Self>, every: Duration) -> JoinHandle<()> {
        let core = Arc::clone(self);
        let defaults = HostLimits::default();
        let last = Mutex::new((defaults.mcp_port, defaults.mcp_tailscale));
        let settings: Arc<Settings> = Arc::new(move || {
            let mut last = lock(&last);
            if let Some(now) = core.mcp_settings() {
                *last = now;
            }
            core.mcp_addresses(last.0, last.1)
        });
        vibe_mcp::http::spawn_listeners(
            Arc::clone(self),
            settings,
            every,
            Arc::clone(&self.mcp_kick),
        )
    }

    /// What `vibe_list_agents` shows of a host's agents: what `agents.list`
    /// does (its memory now and its limit included), with the line that
    /// attaches from another machine.
    pub async fn agent_views(&self, host: &str) -> Result<Vec<AgentView>, ErrorObject> {
        if let Some(link) = self.link(host) {
            // A link that's up but stuck is let go, with what it last listed.
            let (agents, available) =
                match tokio::time::timeout(self.peer_read(), self.remote_agents(&link)).await {
                    Ok(agents) => (agents, link.connected()),
                    Err(_) => (self.kept_agents(&link), false),
                };
            return Ok(agents
                .into_iter()
                .map(|agent| AgentView {
                    // Its attach line is the one that reaches it from here.
                    attach_remote: agent.attach.clone(),
                    available,
                    last_seen: None,
                    agent,
                })
                .collect());
        }
        let list = self
            .agents(HostParams {
                host: Some(host.to_owned()),
            })
            .await?;
        let machine = machine_name();
        let vibe = vibe_path();
        Ok(list
            .agents
            .into_iter()
            .map(|agent| AgentView {
                attach_remote: format!("ssh -t {machine} {vibe} attach {}", agent.agent),
                available: true,
                last_seen: None,
                agent,
            })
            .collect())
    }
}

impl Core {
    /// The Mac, when `host` names it on the Linux host it's behind.
    fn mac_for(&self, host: &str) -> Option<&MacPeer> {
        (host == MAC_HOST).then(|| self.mac()).flatten()
    }
}

impl Core {
    /// `asked`, for as long as another host gets to answer a read, all of it: its
    /// turn at that host as well as the answer. Whatever falls back on what the host
    /// last said then has time to, inside the deadline of whoever's waiting on it.
    async fn within_peer_read<T>(
        &self,
        host: &str,
        asked: impl Future<Output = Result<T, ErrorObject>>,
    ) -> Result<T, ErrorObject> {
        let limit = self.peer_read();
        tokio::time::timeout(limit, asked)
            .await
            .unwrap_or_else(|_| {
                Err(error(
                    ErrorCode::HostUnavailable,
                    format!(
                        "{host} is unavailable: it didn't answer in {} s",
                        limit.as_secs()
                    ),
                ))
            })
    }
}

impl Daemon for Core {
    fn serving(&self, on: bool) {
        if self.mcp_serving.swap(on, Ordering::SeqCst) != on {
            crate::log!("vibed: MCP is {}", if on { "served" } else { "not served" });
            let mut e = self.event(EventKind::McpChanged);
            e.message = Some(
                if on {
                    "MCP is served"
                } else {
                    "MCP isn't served"
                }
                .into(),
            );
            self.publish(e);
        }
    }

    fn log(&self, line: &str) {
        crate::log!("vibed: {line}");
    }

    fn verify_key(&self, secret: &str) -> Option<String> {
        match Core::verify_key(self, secret) {
            Ok(Some(key)) => Some(key.name),
            Ok(None) => None,
            Err(e) => {
                crate::log!("vibed: can't check an API key: {}", e.message);
                None
            }
        }
    }

    fn known_hosts(&self, c: &Caller, agent: &str) -> Vec<String> {
        let mut on = Vec::new();
        if let Some(seen) = self.mac().and_then(|mac| mac.last_seen(c.key()))
            && seen.iter().any(|a| a.agent == agent)
        {
            on.push(MAC_HOST.to_owned());
        }
        let links: Vec<Arc<crate::link::Link>> =
            crate::core::lock(&self.links).values().cloned().collect();
        on.extend(
            links
                .iter()
                .filter(|l| self.kept_agents(l).iter().any(|a| a.agent == agent))
                .map(|l| l.name.clone()),
        );
        on.sort();
        on
    }

    fn known_project_hosts(&self, c: &Caller, project: &str) -> Vec<String> {
        let mut on = Vec::new();
        if self
            .mac()
            .is_some_and(|mac| mac.saw_project(c.key(), project))
        {
            on.push(MAC_HOST.to_owned());
        }
        on.extend(
            crate::core::lock(&self.links)
                .values()
                .filter(|l| lock(&l.projects_seen).iter().any(|p| p == project))
                .map(|l| l.name.clone()),
        );
        on.sort();
        on
    }

    fn peer_deadline(&self) -> Duration {
        self.peer_read()
    }

    async fn hosts(&self) -> Result<Vec<String>, ErrorObject> {
        let mut hosts: Vec<String> = Core::hosts(self)?
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect();
        // The Mac behind this host, while it's the external address.
        if self.mac().is_some() && !hosts.iter().any(|h| h == MAC_HOST) {
            hosts.push(MAC_HOST.to_owned());
        }
        Ok(hosts)
    }

    async fn memory(&self, c: &Caller) -> Vec<(String, vibe_core::rpc::MemoryUse)> {
        let mut all: Vec<_> = Core::hosts(self)
            .map(|l| l.hosts)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|h| Some((h.name, h.memory?)))
            .collect();
        // The Mac behind this host, while it's the external address: its
        // own row, down the tunnel, with the caller's key. While the tunnel
        // is down, or the Mac doesn't take the key, it's left out.
        if let Some(mac) = self.mac()
            && !all.iter().any(|(h, _)| h == MAC_HOST)
        {
            let own = self
                .within_peer_read(
                    MAC_HOST,
                    mac.call(c.key(), HostsList::NAME, json!({}), self.peer_read()),
                )
                .await
                .and_then(typed::<HostList>)
                .ok()
                .and_then(|l| l.hosts.into_iter().find(|h| h.name == LOCAL_HOST))
                .and_then(|h| h.memory);
            if let Some(m) = own {
                all.push((MAC_HOST.to_owned(), m));
            }
        }
        all
    }

    async fn projects(&self, c: &Caller, host: &str) -> Result<Vec<Project>, ErrorObject> {
        if let Some(mac) = self.mac_for(host) {
            let listed = self
                .within_peer_read(
                    host,
                    mac.call(c.key(), ProjectsList::NAME, json!({}), self.peer_read()),
                )
                .await
                .and_then(typed::<ProjectList>);
            return match listed {
                Ok(list) => {
                    mac.saw_projects(c.key(), &list.projects);
                    Ok(list.projects)
                }
                Err(e) => {
                    if e.code == ErrorCode::Unauthorized.code() {
                        mac.forget(c.key());
                    }
                    Err(e)
                }
            };
        }
        if let Some(link) = self.link(host) {
            let params = HostParams::default();
            let asked = self.forward_typed::<ProjectsList>(&link, &params);
            let projects = self.within_peer_read(host, asked).await?.projects;
            *lock(&link.projects_seen) = projects.iter().map(|p| p.name.clone()).collect();
            return Ok(projects);
        }
        Ok(Core::projects(
            self,
            HostParams {
                host: Some(host.to_owned()),
            },
        )?
        .projects)
    }

    async fn agents(&self, c: &Caller, host: &str) -> Result<Vec<AgentView>, ErrorObject> {
        if let Some(mac) = self.mac_for(host) {
            let listed = self
                .within_peer_read(
                    host,
                    mac.call(c.key(), AgentsList::NAME, json!({}), self.peer_read()),
                )
                .await
                .and_then(typed::<AgentList>);
            let (agents, available) = match listed {
                Ok(list) => {
                    mac.saw(c.key(), &list.agents);
                    (list.agents, true)
                }
                // While the tunnel is down, the Mac's agents as this key
                // last saw them, unavailable.
                Err(e) if e.code == ErrorCode::HostUnavailable.code() => {
                    match mac.last_seen(c.key()) {
                        Some(seen) => (seen, false),
                        None => return Err(e),
                    }
                }
                Err(e) => {
                    // The Mac doesn't take this key (any more): what it
                    // listed before isn't its to see.
                    if e.code == ErrorCode::Unauthorized.code() {
                        mac.forget(c.key());
                    }
                    return Err(e);
                }
            };
            return Ok(agents
                .into_iter()
                .map(|agent| AgentView {
                    // It attaches from a terminal on the Mac.
                    attach_remote: format!("on the Mac: {}", agent.attach),
                    available,
                    last_seen: (!available).then(|| mac.seen_at(c.key())).flatten(),
                    agent,
                })
                .collect());
        }
        self.agent_views(host).await
    }

    async fn start_agent(&self, c: &Caller, p: AgentStartParams) -> Result<Agent, ErrorObject> {
        if let Some(mac) = p.host.as_deref().and_then(|h| self.mac_for(h)) {
            return mac_call::<AgentsStart>(mac, c, &p).await;
        }
        if let Some(link) = p.host.as_deref().and_then(|h| self.link(h)) {
            return self.forward_typed::<AgentsStart>(&link, &p).await;
        }
        Core::start_agent(self, p).await
    }

    async fn delete_agent(&self, c: &Caller, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        if let Some(mac) = p.host.as_deref().and_then(|h| self.mac_for(h)) {
            return mac_call::<AgentsDelete>(mac, c, &p).await;
        }
        if let Some(link) = p.host.as_deref().and_then(|h| self.link(h)) {
            return self.forward_typed::<AgentsDelete>(&link, &p).await;
        }
        Core::delete_agent(self, p).await
    }

    async fn send(
        &self,
        c: &Caller,
        t: &Target,
        input: Input,
        request_id: &str,
    ) -> Result<Sent, ErrorObject> {
        // It's this key's id: another key's the same one is another send.
        let request_id = vibe_core::keys::hash(&format!("{}\n{request_id}", c.key()));
        let request_id = request_id.as_str();
        let mac = self.mac_for(&t.host);
        let link = self.link(&t.host);
        if mac.is_some() || link.is_some() {
            let params = crate::talk::send_params(&t.host, &t.agent, &input, request_id);
            let sent = match mac {
                Some(mac) => {
                    let params = serde_json::to_value(&params).map_err(|e| {
                        error(ErrorCode::InternalError, format!("Internal error: {e}"))
                    })?;
                    let answer = mac
                        .call_at(
                            c.key(),
                            AgentsSend::NAME,
                            params,
                            call_timeout(AgentsSend::NAME),
                            Some(self.now()),
                        )
                        .await?;
                    typed::<AgentSent>(answer)?
                }
                None => {
                    let link = link.expect("one of them");
                    self.forward_typed::<AgentsSend>(&link, &params).await?
                }
            };
            return Ok(Sent {
                woke: sent.woke,
                state: sent.state,
            });
        }
        self.send_once(Some(&t.host), &t.agent, input, request_id)
            .await
    }

    async fn check(&self, c: &Caller, t: &Target, background: bool) -> Result<Check, ErrorObject> {
        let params = AgentCheckParams {
            host: Some(t.host.clone()),
            agent: t.agent.clone(),
            background: background.then_some(true),
        };
        let checked = if let Some(mac) = self.mac_for(&t.host) {
            mac_call::<AgentsCheck>(mac, c, &params).await?
        } else if let Some(link) = self.link(&t.host) {
            self.forward_typed::<AgentsCheck>(&link, &params).await?
        } else {
            return self.check_agent(Some(&t.host), &t.agent, background).await;
        };
        Ok(Check {
            state: checked.state,
            activity: checked.activity,
            line: checked.line,
            answered: checked.answered,
        })
    }

    async fn read_output(
        &self,
        c: &Caller,
        t: &Target,
        after: Option<u64>,
    ) -> Result<Screen, ErrorObject> {
        let params = AgentOutputParams {
            host: Some(t.host.clone()),
            agent: t.agent.clone(),
            after,
        };
        let o = if let Some(mac) = self.mac_for(&t.host) {
            mac_call::<AgentsOutput>(mac, c, &params).await?
        } else if let Some(link) = self.link(&t.host) {
            self.forward_typed::<AgentsOutput>(&link, &params).await?
        } else {
            return self.agent_output(Some(&t.host), &t.agent, after).await;
        };
        Ok(Screen {
            screen: o.screen,
            history: o.history,
            cursor: o.cursor,
            dropped: o.dropped,
            more: o.more,
            live: o.live,
        })
    }

    async fn read_messages(
        &self,
        c: &Caller,
        t: &Target,
        last: usize,
    ) -> Result<Vec<Message>, ErrorObject> {
        let params = AgentMessagesParams {
            host: Some(t.host.clone()),
            agent: t.agent.clone(),
            last: Some(u32::try_from(last).unwrap_or(u32::MAX)),
        };
        if let Some(mac) = self.mac_for(&t.host) {
            return Ok(mac_call::<AgentsMessages>(mac, c, &params).await?.messages);
        }
        if let Some(link) = self.link(&t.host) {
            return Ok(self
                .forward_typed::<AgentsMessages>(&link, &params)
                .await?
                .messages);
        }
        self.agent_messages(Some(&t.host), &t.agent, last).await
    }
}
