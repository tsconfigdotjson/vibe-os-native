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
    AgentStartParams, AgentsCheck, AgentsDelete, AgentsList, AgentsMessages, AgentsOutput,
    AgentsSend, AgentsStart, ErrorCode, ErrorObject, HostList, HostParams, HostsList, McpSetParams,
    McpSettings, Method, Project, ProjectList, ProjectsList,
};
use vibe_mcp::http::Settings;
use vibe_mcp::{AgentView, Caller, Check, Daemon, Input, Message, Screen, Sent, Target};

use crate::core::{Core, error, invalid, limits_error, lock};
use crate::hosts::call_timeout;
use crate::mac_peer::MacPeer;

/// A value the Mac answered with, as `T`.
fn typed<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, ErrorObject> {
    serde_json::from_value(v).map_err(|e| {
        error(
            ErrorCode::InternalError,
            format!("Internal error: the Mac answered with something unexpected: {e}"),
        )
    })
}

/// Whose view of `host` this is, for what's kept of hosts reached through the Mac: the
/// caller's of the Mac itself, and apart for each host linked to it.
fn seen_as(c: &Caller, host: &str) -> String {
    if host == MAC_HOST {
        c.key().to_owned()
    } else {
        format!("{}\n{host}", c.key())
    }
}

/// What a call for `agent` through the Mac says of it: there, or not there any more.
fn note_mac_answer<T>(mac: &MacPeer, seen: &str, agent: &str, answer: &Result<T, ErrorObject>) {
    match answer {
        Ok(_) => mac.note_ref(seen, agent),
        Err(e) if e.code == ErrorCode::NotFound.code() => mac.forget_ref(seen, agent),
        Err(_) => {}
    }
}

/// Calls `M` through the Mac behind this host for `c`, whose key the Mac checks.
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
    /// The other hosts linked to the Mac, as it says now, with `c`'s key, which it checks.
    /// Not asked if it's known to be away: what `c` last heard stands. Nor does what's
    /// asked hold anything up for longer than another host gets to answer a read.
    async fn linked_through_mac(&self, c: &Caller, mac: &MacPeer) -> Vec<String> {
        if let Some(known) = mac.linked(c.key()) {
            return known;
        }
        if !mac.is_up() {
            return Vec::new();
        }
        let asked = self
            .within_peer_read(
                MAC_HOST,
                mac.call(c.key(), HostsList::NAME, json!({}), self.peer_read()),
            )
            .await
            .and_then(typed::<HostList>);
        let Ok(list) = asked else {
            return Vec::new();
        };
        let names: Vec<String> = list
            .hosts
            .into_iter()
            .map(|h| h.name)
            .filter(|h| h != LOCAL_HOST && h != MAC_HOST)
            .collect();
        mac.saw_linked(c.key(), names.clone());
        names
    }

    /// The Mac, when `host` is reached through it on the Linux host it's behind: the Mac
    /// itself, or another host linked to it. One linked here is this host's to reach.
    async fn mac_for(&self, c: &Caller, host: &str) -> Option<&MacPeer> {
        let mac = self.mac()?;
        if host == MAC_HOST {
            return Some(mac);
        }
        if host == LOCAL_HOST || self.link(host).is_some() {
            return None;
        }
        self.linked_through_mac(c, mac)
            .await
            .iter()
            .any(|h| h == host)
            .then_some(mac)
    }
}

impl Core {
    /// [`mac_call`] for a send, start or delete, saying when it was sent, as the Mac
    /// refuses one that took too long to arrive.
    async fn mac_change<M: Method>(
        &self,
        mac: &MacPeer,
        c: &Caller,
        params: &M::Params,
    ) -> Result<M::Result, ErrorObject> {
        let params = serde_json::to_value(params)
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?;
        let v = mac
            .call_at(
                c.key(),
                M::NAME,
                params,
                call_timeout(M::NAME),
                Some(self.now()),
            )
            .await?;
        typed(v)
    }

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
        if let Some(mac) = self.mac() {
            if mac.knows_ref(c.key(), agent) {
                on.push(MAC_HOST.to_owned());
            }
            for host in mac.linked(c.key()).unwrap_or_default() {
                if mac.knows_ref(&seen_as(c, &host), agent) {
                    on.push(host);
                }
            }
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
        if let Some(mac) = self.mac() {
            if mac.saw_project(c.key(), project) {
                on.push(MAC_HOST.to_owned());
            }
            for host in mac.linked(c.key()).unwrap_or_default() {
                if mac.saw_project(&seen_as(c, &host), project) {
                    on.push(host);
                }
            }
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

    async fn hosts(&self, c: &Caller) -> Result<Vec<String>, ErrorObject> {
        let mut hosts: Vec<String> = Core::hosts(self)?
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect();
        // The Mac behind this host, and what's linked to it.
        if let Some(mac) = self.mac() {
            if !hosts.iter().any(|h| h == MAC_HOST) {
                hosts.push(MAC_HOST.to_owned());
            }
            for host in self.linked_through_mac(c, mac).await {
                if !hosts.contains(&host) {
                    hosts.push(host);
                }
            }
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
        // The Mac behind this host: what came with
        // the caller's last listing of it. If that couldn't be had, or the Mac doesn't
        // take the key, it's left out. An older Mac doesn't send it, and is asked.
        if let Some(mac) = self.mac()
            && !all.iter().any(|(h, _)| h == MAC_HOST)
        {
            let own = if mac.accepts_calls() {
                mac.memory_seen(c.key())
            } else {
                self.within_peer_read(
                    MAC_HOST,
                    mac.call(c.key(), HostsList::NAME, json!({}), self.peer_read()),
                )
                .await
                .and_then(typed::<HostList>)
                .ok()
                .and_then(|l| l.hosts.into_iter().find(|h| h.name == LOCAL_HOST))
                .and_then(|h| h.memory)
            };
            if let Some(m) = own {
                all.push((MAC_HOST.to_owned(), m));
            }
            // What came with the caller's last listing of each host linked to it.
            for host in mac.linked(c.key()).unwrap_or_default() {
                if !all.iter().any(|(h, _)| *h == host)
                    && let Some(m) = mac.memory_seen(&seen_as(c, &host))
                {
                    all.push((host, m));
                }
            }
        }
        all
    }

    async fn projects(&self, c: &Caller, host: &str) -> Result<Vec<Project>, ErrorObject> {
        if let Some(mac) = self.mac_for(c, host).await {
            let seen = seen_as(c, host);
            let listed = self
                .within_peer_read(
                    host,
                    mac.call(
                        c.key(),
                        ProjectsList::NAME,
                        json!({"host": host}),
                        self.peer_read(),
                    ),
                )
                .await
                .and_then(typed::<ProjectList>);
            return match listed {
                Ok(list) => {
                    mac.saw_projects(&seen, &list.projects);
                    Ok(list.projects)
                }
                Err(e) => {
                    if e.code == ErrorCode::Unauthorized.code() {
                        mac.forget(&seen);
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
        if let Some(mac) = self.mac_for(c, host).await {
            let seen = seen_as(c, host);
            let listed = self
                .within_peer_read(
                    host,
                    mac.call(
                        c.key(),
                        AgentsList::NAME,
                        json!({"host": host}),
                        self.peer_read(),
                    ),
                )
                .await
                .and_then(|answer| {
                    let memory = answer
                        .get("memory")
                        .and_then(|m| serde_json::from_value(m.clone()).ok());
                    // What else is linked to it keeps up with every listing.
                    if let Some(linked) = answer
                        .get("linked_hosts")
                        .and_then(|l| serde_json::from_value::<Vec<String>>(l.clone()).ok())
                    {
                        mac.saw_linked(c.key(), linked);
                    }
                    Ok((typed::<AgentList>(answer)?, memory))
                });
            let memory = match &listed {
                Ok((_, memory)) => memory.clone(),
                Err(_) => None,
            };
            mac.saw_memory(&seen, memory);
            let (agents, available) = match listed.map(|(list, _)| list) {
                Ok(list) => {
                    mac.saw(&seen, &list.agents);
                    (list.agents, true)
                }
                // While the tunnel or that host is down, its agents as this
                // key last saw them, unavailable.
                Err(e) if e.code == ErrorCode::HostUnavailable.code() => {
                    match mac.last_seen(&seen) {
                        Some(seen) => (seen, false),
                        None => return Err(e),
                    }
                }
                Err(e) => {
                    // The Mac doesn't take this key (any more): what it
                    // listed before isn't its to see.
                    if e.code == ErrorCode::Unauthorized.code() {
                        mac.forget(&seen);
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
                    last_seen: (!available).then(|| mac.seen_at(&seen)).flatten(),
                    agent,
                })
                .collect());
        }
        self.agent_views(host).await
    }

    async fn start_agent(&self, c: &Caller, p: AgentStartParams) -> Result<Agent, ErrorObject> {
        let host = p.host.clone().unwrap_or_default();
        if let Some(mac) = self.mac_for(c, &host).await {
            let started = self.mac_change::<AgentsStart>(mac, c, &p).await?;
            mac.note_ref(&seen_as(c, &host), &started.agent);
            return Ok(started);
        }
        if let Some(link) = p.host.as_deref().and_then(|h| self.link(h)) {
            let started = self.forward_typed::<AgentsStart>(&link, &p).await?;
            self.note_kept_agent(&link, &started);
            return Ok(started);
        }
        Core::start_agent(self, p).await
    }

    async fn delete_agent(&self, c: &Caller, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        let host = p.host.clone().unwrap_or_default();
        if let Some(mac) = self.mac_for(c, &host).await {
            let deleted = self.mac_change::<AgentsDelete>(mac, c, &p).await?;
            mac.forget_ref(&seen_as(c, &host), &deleted.agent);
            return Ok(deleted);
        }
        if let Some(link) = p.host.as_deref().and_then(|h| self.link(h)) {
            let deleted = self.forward_typed::<AgentsDelete>(&link, &p).await?;
            self.forget_kept_agent(&link, &deleted.agent);
            return Ok(deleted);
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
        let mac = self.mac_for(c, &t.host).await;
        let link = self.link(&t.host);
        if mac.is_some() || link.is_some() {
            let params = crate::talk::send_params(&t.host, &t.agent, &input, request_id);
            let sent = match mac {
                Some(mac) => {
                    let sent = self.mac_change::<AgentsSend>(mac, c, &params).await;
                    note_mac_answer(mac, &seen_as(c, &t.host), &t.agent, &sent);
                    sent?
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
        let checked = if let Some(mac) = self.mac_for(c, &t.host).await {
            let checked = mac_call::<AgentsCheck>(mac, c, &params).await;
            note_mac_answer(mac, &seen_as(c, &t.host), &t.agent, &checked);
            checked?
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
        let o = if let Some(mac) = self.mac_for(c, &t.host).await {
            let output = mac_call::<AgentsOutput>(mac, c, &params).await;
            note_mac_answer(mac, &seen_as(c, &t.host), &t.agent, &output);
            output?
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
        if let Some(mac) = self.mac_for(c, &t.host).await {
            let read = mac_call::<AgentsMessages>(mac, c, &params).await;
            note_mac_answer(mac, &seen_as(c, &t.host), &t.agent, &read);
            return Ok(read?.messages);
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
