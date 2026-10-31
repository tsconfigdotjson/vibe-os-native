//! MCP on this daemon's core (PRD: MCP interface). `vibe-mcp` is the
//! façade; this gives it the same handlers the socket API calls, so the two
//! can't drift apart.
//!
//! Only this host's agents are reachable until Linux hosts (#16) connect
//! others: a call for another host gets the core's "no host named …".

use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use vibe_core::limits::{self, HostLimits};
use vibe_core::model::{AgentState, EventKind};
use vibe_core::rpc::{
    Agent, AgentDeleteParams, AgentStartParams, ErrorObject, HostParams, McpSetParams, McpSettings,
    Project,
};
use vibe_mcp::http::Settings;
use vibe_mcp::{AgentView, Check, Daemon, Input, Message, Screen, Sent, Target};

use crate::agents::blocking;
use crate::core::{Core, invalid, limits_error, lock};

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
    /// does, with each one's memory now and its limit.
    pub async fn agent_views(&self, host: &str) -> Result<Vec<AgentView>, ErrorObject> {
        let list = self
            .agents(HostParams {
                host: Some(host.to_owned()),
            })
            .await?;
        let machine = machine_name();
        let vibe = vibe_path();
        let mut views = Vec::new();
        for agent in list.agents {
            let record = self.store().agent(&agent.host, &agent.agent).ok();
            let memory_limit = record.as_ref().map_or(0, |r| r.memory);
            let awake = !matches!(agent.state, AgentState::Asleep | AgentState::Exited);
            let memory = match (&record, awake) {
                (Some(r), true) => self.memory_of(r).await,
                _ => None,
            };
            views.push(AgentView {
                attach_remote: format!("ssh -t {machine} {vibe} attach {}", agent.agent),
                agent,
                memory,
                memory_limit,
            });
        }
        Ok(views)
    }

    /// What `r`'s compartment uses now, if its backend can say.
    async fn memory_of(&self, r: &vibe_core::store::AgentRecord) -> Option<u64> {
        let backend = self.backend(r.environment).ok()?;
        let handle = Core::handle_of(r).ok()?;
        blocking(move || backend.usage(&handle))
            .await
            .ok()
            .map(|u| u.memory)
    }
}

impl Daemon for Core {
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

    async fn hosts(&self) -> Result<Vec<String>, ErrorObject> {
        Ok(Core::hosts(self)?
            .hosts
            .into_iter()
            .map(|h| h.name)
            .collect())
    }

    async fn projects(&self, host: &str) -> Result<Vec<Project>, ErrorObject> {
        Ok(Core::projects(
            self,
            HostParams {
                host: Some(host.to_owned()),
            },
        )?
        .projects)
    }

    async fn agents(&self, host: &str) -> Result<Vec<AgentView>, ErrorObject> {
        self.agent_views(host).await
    }

    async fn start_agent(&self, p: AgentStartParams) -> Result<Agent, ErrorObject> {
        Core::start_agent(self, p).await
    }

    async fn delete_agent(&self, p: AgentDeleteParams) -> Result<Agent, ErrorObject> {
        Core::delete_agent(self, p).await
    }

    async fn send(&self, t: &Target, input: Input) -> Result<Sent, ErrorObject> {
        self.send_input(Some(&t.host), &t.agent, input).await
    }

    async fn check(&self, t: &Target, background: bool) -> Result<Check, ErrorObject> {
        self.check_agent(Some(&t.host), &t.agent, background).await
    }

    async fn read_output(&self, t: &Target, after: Option<u64>) -> Result<Screen, ErrorObject> {
        self.agent_output(Some(&t.host), &t.agent, after).await
    }

    async fn read_messages(&self, t: &Target, last: usize) -> Result<Vec<Message>, ErrorObject> {
        self.agent_messages(Some(&t.host), &t.agent, last).await
    }
}
