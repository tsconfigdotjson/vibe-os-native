//! External access (PRD: Linux hosts and external access, External
//! access), on both ends.
//!
//! On a Linux host that's the external address, `vibed` serves MCP
//! publicly at `https://<domain>/mcp`, with a certificate from Let's
//! Encrypt (see [`crate::acme`]), every call rate-limited and logged (see
//! [`vibe_mcp::public`]). Behind it is the Mac, which keeps an SSH reverse
//! tunnel to a Unix socket in the host's private `run/` (see
//! [`crate::tunnel`]). Outside callers see the Mac's agents as `mac:<ref>`;
//! a call for one goes down the tunnel with the caller's key, and the Mac
//! checks the key itself, so a host that's been taken over can't drive the
//! Mac without one. While the Mac can't be reached, the host answers for
//! its own agents, and the Mac's are unavailable.
//!
//! On the Mac, `hosts.external` records which Linux host is the external
//! address (one at most), tells the host, and keeps the tunnel to it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::{TcpListener, UnixStream};
use tokio::task::JoinHandle;
use vibe_core::model::{EventKind, LOCAL_HOST, MAC_HOST, TunnelState, validate_domain};
use vibe_core::rpc::{ACCEPTS_CALLS, MemoryUse};
use vibe_core::rpc::{
    Agent, ErrorCode, ErrorObject, HostExternalParams, HostInfo, HostsExternal, Method, Project,
    TunnelPrepared,
};
use vibe_mcp::public::{Limiter, Limits};

use crate::acme::{self, Certs, Disk};
use crate::core::{Core, error, invalid, lock, store_error};
use crate::remote::{ACCEPT_WITHIN, Channel};

/// The port the public endpoint listens on: 443, which Let's Encrypt's
/// TLS-ALPN-01 challenge comes to.
pub const HTTPS_PORT: u16 = 443;

/// Where the Mac's tunnel ends on a Linux host, in its private `run/`.
pub const TUNNEL_END: &str = "mac.sock";

/// How the external address is served on a Linux host. Tests change all of
/// it; `vibed` takes the port, the ACME server and a fixed certificate from
/// the environment (see its `--help`).
#[derive(Debug, Clone)]
pub struct ExternalConfig {
    /// Whether this host may serve the external address: a Linux host may,
    /// a Mac never ([`crate::core::Config::new`] says which by its OS).
    pub serve_here: bool,
    pub port: u16,
    /// The one address to listen on; every address by default. Tests keep
    /// to loopback.
    pub listen: Option<std::net::IpAddr>,
    /// The ACME directory certificates come from: Let's Encrypt's.
    pub acme_directory: String,
    /// A PEM certificate chain and key to serve instead of one from ACME.
    pub fixed_cert: Option<(PathBuf, PathBuf)>,
    /// How often the tunnel's end is looked at for a Mac.
    pub probe_every: Duration,
    /// How hard the public endpoint is on each client.
    pub limits: Limits,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        ExternalConfig {
            serve_here: false,
            port: HTTPS_PORT,
            listen: None,
            acme_directory: acme::LETS_ENCRYPT.into(),
            fixed_cert: None,
            probe_every: Duration::from_secs(5),
            limits: Limits::default(),
        }
    }
}

/// The public endpoint, while this Linux host is the external address.
#[derive(Debug)]
pub(crate) struct Public {
    domain: String,
    task: JoinHandle<()>,
    /// What keeps it from working, or holds it up, if anything does.
    detail: Arc<Mutex<Option<String>>>,
}

impl Drop for Public {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Aborts a task when it's dropped, as when the task that started it is.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

// The Mac, from the host.

/// The Mac behind this host, reached down its tunnel.
#[derive(Debug)]
pub(crate) struct MacPeer {
    socket: PathBuf,
    /// Whether the Mac answered through the tunnel when last looked. Only
    /// looks clear it: one failed call says nothing of the others.
    up: AtomicBool,
    /// Looks that have failed in a row.
    misses: AtomicU32,
    /// Whether the Mac's `vibed` is one that sends `call.accepted`, as its hello
    /// says, and gives its memory with its agents. An older one does neither.
    accepts_calls: AtomicBool,
    idle: Mutex<Vec<(Channel, Instant)>>,
    /// Calls to the Mac at once, fewer than its tunnel socket serves, so
    /// one is always left for the look that says it's there.
    calls: tokio::sync::Semaphore,
    /// The Mac's agents as each key last listed them, by the key's hash, to
    /// show unavailable while the tunnel is down. Only to that key: one the
    /// Mac doesn't take never sees them.
    seen: Mutex<HashMap<String, (i64, Vec<Agent>)>>,
    /// The Mac's projects as each key last listed them, likewise: a project
    /// there is still there while the Mac is away.
    projects_seen: Mutex<HashMap<String, Vec<String>>>,
    /// Refs each key has seen answer since its last listing, as a start's did, so a
    /// bare ref goes straight there.
    refs_seen: Mutex<HashMap<String, HashSet<String>>>,
    /// The Mac's memory as it came with each key's last listing.
    memory_seen: Mutex<HashMap<String, MemoryUse>>,
}

/// How long a call waits its turn at the Mac, as stuck calls mustn't queue new
/// ones for as long as they're stuck.
const TURN_WITHIN: Duration = Duration::from_secs(3);

/// How often a call under way looks at whether the Mac's been found gone.
const STILL_THERE_EVERY: Duration = Duration::from_millis(250);

/// Looks that fail in a row before calls under way stop waiting. One can be a host
/// stalling for a moment, and giving up on a call that's fine costs its caller a
/// guess at whether it happened.
const GONE_AFTER: u32 = 2;

/// Channels to the Mac kept between calls, and for how long.
const MAX_IDLE: usize = 4;
const IDLE_FOR: Duration = Duration::from_secs(30);

/// Keys whose last listing of the Mac's agents is kept.
const MAX_SEEN: usize = 64;

/// Calls to the Mac at once: the Mac's tunnel socket serves 16.
const MAX_CALLS: usize = 8;

/// What to do about a call that changes something, once it's gone out and nobody
/// has heard how it ended. Nothing, for one that doesn't.
pub(crate) fn check_after(method: &str) -> Option<&'static str> {
    match method {
        "agents.send" => Some("check vibe_read_messages before resending"),
        "agents.start" | "agents.delete" => Some("check vibe_list_agents before trying again"),
        _ => None,
    }
}

/// What a call gets that never went out.
fn not_sent(method: &str, why: &str) -> ErrorObject {
    if check_after(method).is_some() {
        mac_unavailable(&format!("{why}, so nothing was sent"))
    } else {
        mac_unavailable(why)
    }
}

/// What a call gets that went out and hasn't been heard of since.
fn lost(method: &str, why: &str) -> ErrorObject {
    match check_after(method) {
        Some(check) => error(
            ErrorCode::HostUnavailable,
            format!("{MAC_HOST} is unavailable: {why}. Outcome unknown: {check}"),
        )
        .outcome_unknown(),
        None => mac_unavailable(why),
    }
}

/// What a call for the Mac gets while the tunnel is down.
fn mac_unavailable(why: &str) -> ErrorObject {
    error(
        ErrorCode::HostUnavailable,
        format!(
            "{MAC_HOST} is unavailable: {why}. The Mac may be asleep or offline; its agents come back when its tunnel does"
        ),
    )
}

/// The Mac's answer, as this host says it: every `host` in it is `mac`.
fn relabel(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.contains_key("host") {
                map.insert("host".into(), json!(MAC_HOST));
            }
            map.values_mut().for_each(relabel);
        }
        Value::Array(items) => items.iter_mut().for_each(relabel),
        _ => {}
    }
}

impl MacPeer {
    pub(crate) fn new(socket: PathBuf) -> MacPeer {
        MacPeer {
            socket,
            up: AtomicBool::new(false),
            misses: AtomicU32::new(0),
            accepts_calls: AtomicBool::new(false),
            idle: Mutex::new(Vec::new()),
            calls: tokio::sync::Semaphore::new(MAX_CALLS),
            seen: Mutex::new(HashMap::new()),
            projects_seen: Mutex::new(HashMap::new()),
            refs_seen: Mutex::new(HashMap::new()),
            memory_seen: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
    }

    /// Forgets everything about the Mac, as when the tunnel's end is made
    /// anew or external access goes off.
    pub(crate) fn reset(&self) {
        lock(&self.idle).clear();
        self.forget_all();
        self.up.store(false, Ordering::SeqCst);
        self.misses.store(GONE_AFTER, Ordering::SeqCst);
    }

    /// What a look at the tunnel found. A miss stops new calls at once; only
    /// [`GONE_AFTER`] of them stop those under way.
    fn note_look(&self, up: bool) {
        if up {
            self.misses.store(0, Ordering::SeqCst);
        } else {
            self.misses.fetch_add(1, Ordering::SeqCst);
            // Whatever's kept leads to something that isn't answering.
            lock(&self.idle).clear();
        }
        self.up.store(up, Ordering::SeqCst);
    }

    async fn dial(&self) -> std::io::Result<Channel> {
        let stream = UnixStream::connect(&self.socket).await?;
        let (read, write) = stream.into_split();
        Ok(Channel::new(
            BufReader::new(read),
            write,
            (),
            Arc::new(Mutex::new(String::new())),
        ))
    }

    /// Whether the Mac answers through the tunnel now: its `daemon.hello`.
    pub(crate) async fn probe(&self) -> bool {
        let hello = async {
            let mut c = self.dial().await.ok()?;
            let answer = c
                .call(
                    vibe_core::rpc::Hello::NAME,
                    json!({"client": "vibed tunnel probe", "protocol": vibe_core::rpc::PROTOCOL_VERSION}),
                )
                .await
                .ok()?
                .ok()?;
            Some(answer)
        };
        let answer = tokio::time::timeout(Duration::from_secs(5), hello)
            .await
            .ok()
            .flatten();
        if let Some(answer) = &answer {
            self.accepts_calls.store(
                answer.get(ACCEPTS_CALLS) == Some(&Value::Bool(true)),
                Ordering::SeqCst,
            );
        }
        let up = answer.is_some();
        self.note_look(up);
        up
    }

    /// Calls `method` on the Mac for the caller with `key`, which the Mac
    /// checks itself, and gives its answer as this host says it. `params`
    /// name no host: the tunnel reaches the Mac's own agents only.
    pub(crate) async fn call(
        &self,
        key: &str,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, ErrorObject> {
        self.call_at(key, method, params, timeout, None).await
    }

    /// [`MacPeer::call`], saying when it was sent, as the Mac refuses a send, start
    /// or delete that took too long to arrive.
    pub(crate) async fn call_at(
        &self,
        key: &str,
        method: &str,
        mut params: Value,
        timeout: Duration,
        sent_at: Option<i64>,
    ) -> Result<Value, ErrorObject> {
        if let Some(map) = params.as_object_mut() {
            map.remove("host");
        }
        // Asleep, the end of its tunnel may accept and then say nothing: once the
        // look at it has found that, nothing more is sent there to wait for.
        if !self.is_up() {
            return Err(not_sent(
                method,
                "it didn't answer the last look at its tunnel",
            ));
        }
        let mut message = json!({"key": key, "method": method, "params": params});
        if let Some(at) = sent_at {
            message["sent_at"] = json!(at);
        }
        let _turn = tokio::time::timeout(TURN_WITHIN, self.calls.acquire())
            .await
            .map_err(|_| not_sent(method, "too many calls to it are stuck"))?
            .map_err(|_| not_sent(method, "its tunnel is closing"))?;
        // A channel kept from before may lead to a tunnel since replaced: one
        // that fails is followed by a fresh one, once. Not for a call that
        // changes something, which may have got there before the channel failed
        // and mustn't go twice: that always goes on a fresh one.
        let kept = if check_after(method).is_some() {
            None
        } else {
            let mut idle = lock(&self.idle);
            idle.retain(|(_, since)| since.elapsed() < IDLE_FOR);
            idle.pop().map(|(c, _)| c)
        };
        let mut attempts = kept.into_iter().map(Some).chain([None]);
        loop {
            let Some(next) = attempts.next() else {
                return Err(not_sent(method, "its tunnel is down"));
            };
            let fresh = next.is_none();
            let mut channel = match next {
                Some(c) => c,
                None => match self.dial().await {
                    Ok(c) => c,
                    Err(_) => return Err(not_sent(method, "its tunnel is down")),
                },
            };
            let accepting = self.accepts_calls.load(Ordering::SeqCst);
            let waiting = tokio::time::timeout(timeout, async {
                if accepting {
                    channel
                        .call_accepted("tunnel.call", message.clone(), ACCEPT_WITHIN)
                        .await
                } else {
                    channel.call("tunnel.call", message.clone()).await
                }
            });
            let answer = tokio::select! {
                answer = waiting => answer,
                () = self.found_gone() => return Err(lost(method, "it stopped answering")),
            };
            let outcome = match answer {
                Err(_) => {
                    return Err(lost(
                        method,
                        &format!("it didn't answer {method} in {} s", timeout.as_secs()),
                    ));
                }
                // Nothing's known of it: what's kept may be fine, and the Mac not.
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::TimedOut => {
                    return Err(lost(method, &format!("{e}").replace("tunnel.call", method)));
                }
                Ok(Err(_)) if !fresh => continue,
                Ok(Err(e)) => return Err(lost(method, &format!("its tunnel failed: {e}"))),
                Ok(Ok(outcome)) => outcome,
            };
            self.note_look(true);
            {
                let mut idle = lock(&self.idle);
                if idle.len() < MAX_IDLE {
                    idle.push((channel, Instant::now()));
                }
            }
            return match outcome {
                Ok(mut v) => {
                    relabel(&mut v);
                    Ok(v)
                }
                Err(mut e) => {
                    e.message = e
                        .message
                        .replace(&format!(" on {LOCAL_HOST}"), &format!(" on {MAC_HOST}"));
                    Err(e)
                }
            };
        }
    }

    /// Ends once looks at the tunnel have found the Mac gone.
    async fn found_gone(&self) {
        while self.misses.load(Ordering::SeqCst) < GONE_AFTER {
            tokio::time::sleep(STILL_THERE_EVERY).await;
        }
    }

    /// Keeps which projects `key` last listed on the Mac.
    pub(crate) fn saw_projects(&self, key: &str, projects: &[Project]) {
        let mut seen = lock(&self.projects_seen);
        let k = vibe_core::keys::hash(key);
        if !seen.contains_key(&k) && seen.len() >= MAX_SEEN {
            seen.clear();
        }
        seen.insert(k, projects.iter().map(|p| p.name.clone()).collect());
    }

    /// Whether `key` last listed `project` on the Mac.
    pub(crate) fn saw_project(&self, key: &str, project: &str) -> bool {
        lock(&self.projects_seen)
            .get(&vibe_core::keys::hash(key))
            .is_some_and(|names| names.iter().any(|n| n == project))
    }

    /// Whether the Mac is one that gives its memory with its agents.
    pub(crate) fn accepts_calls(&self) -> bool {
        self.accepts_calls.load(Ordering::SeqCst)
    }

    /// Keeps the memory that came with `key`'s listing, or forgets it, if that
    /// listing couldn't be had.
    pub(crate) fn saw_memory(&self, key: &str, memory: Option<MemoryUse>) {
        let mut seen = lock(&self.memory_seen);
        let k = vibe_core::keys::hash(key);
        match memory {
            Some(memory) => {
                if !seen.contains_key(&k) && seen.len() >= MAX_SEEN {
                    seen.clear();
                }
                seen.insert(k, memory);
            }
            None => {
                seen.remove(&k);
            }
        }
    }

    /// The memory that came with `key`'s last listing.
    pub(crate) fn memory_seen(&self, key: &str) -> Option<MemoryUse> {
        lock(&self.memory_seen)
            .get(&vibe_core::keys::hash(key))
            .cloned()
    }

    /// Notes that `agent` is on the Mac, as `key` has just seen it answer.
    pub(crate) fn note_ref(&self, key: &str, agent: &str) {
        let mut seen = lock(&self.refs_seen);
        let k = vibe_core::keys::hash(key);
        if !seen.contains_key(&k) && seen.len() >= MAX_SEEN {
            seen.clear();
        }
        seen.entry(k).or_default().insert(agent.to_owned());
    }

    /// Notes that `agent` is gone from the Mac, as `key` has just seen.
    pub(crate) fn forget_ref(&self, key: &str, agent: &str) {
        let k = vibe_core::keys::hash(key);
        if let Some(refs) = lock(&self.refs_seen).get_mut(&k) {
            refs.remove(agent);
        }
        if let Some((_, agents)) = lock(&self.seen).get_mut(&k) {
            agents.retain(|a| a.agent != agent);
        }
    }

    /// Whether `key` knows of `agent` on the Mac, from a listing or anything since.
    pub(crate) fn knows_ref(&self, key: &str, agent: &str) -> bool {
        let k = vibe_core::keys::hash(key);
        lock(&self.refs_seen)
            .get(&k)
            .is_some_and(|refs| refs.contains(agent))
            || lock(&self.seen)
                .get(&k)
                .is_some_and(|(_, agents)| agents.iter().any(|a| a.agent == agent))
    }

    /// Keeps what `key` last listed of the Mac's agents.
    pub(crate) fn saw(&self, key: &str, agents: &[Agent]) {
        // That's all of them now: what was noted since is in it, or has gone.
        lock(&self.refs_seen).remove(&vibe_core::keys::hash(key));
        let mut seen = lock(&self.seen);
        let k = vibe_core::keys::hash(key);
        if !seen.contains_key(&k) && seen.len() >= MAX_SEEN {
            seen.clear();
        }
        seen.insert(k, (vibe_core::time::now(), agents.to_vec()));
    }

    /// Forgets what `key` listed, as when the Mac no longer takes it.
    pub(crate) fn forget(&self, key: &str) {
        let k = vibe_core::keys::hash(key);
        lock(&self.seen).remove(&k);
        lock(&self.projects_seen).remove(&k);
        lock(&self.refs_seen).remove(&k);
        lock(&self.memory_seen).remove(&k);
    }

    /// Forgets what every key listed, as when the keys change.
    pub(crate) fn forget_all(&self) {
        lock(&self.seen).clear();
        lock(&self.projects_seen).clear();
        lock(&self.refs_seen).clear();
        lock(&self.memory_seen).clear();
    }

    /// What `key` last listed of the Mac's agents, if it listed them.
    pub(crate) fn last_seen(&self, key: &str) -> Option<Vec<Agent>> {
        lock(&self.seen)
            .get(&vibe_core::keys::hash(key))
            .map(|(_, agents)| agents.clone())
    }

    /// When `key` last listed the Mac's agents.
    pub(crate) fn seen_at(&self, key: &str) -> Option<String> {
        lock(&self.seen)
            .get(&vibe_core::keys::hash(key))
            .map(|(at, _)| vibe_core::time::rfc3339(*at))
    }
}

/// The public endpoint's listener on `port`: every address, IPv6 and IPv4
/// alike where the machine has IPv6, else IPv4; or only `listen`.
async fn bind_public(
    listen: Option<std::net::IpAddr>,
    port: u16,
    domain: &str,
) -> Result<TcpListener, String> {
    let bound = match listen {
        Some(ip) => TcpListener::bind((ip, port)).await,
        None => match dual_stack(port) {
            Ok(l) => Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(e),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => Err(e),
            Err(_) => TcpListener::bind((std::net::Ipv4Addr::UNSPECIFIED, port)).await,
        },
    };
    bound.map_err(|e| match e.kind() {
        std::io::ErrorKind::PermissionDenied => format!(
            "can't listen on port {port}: vibed runs as you, and Linux keeps ports below 1024 for \
             root. Allow it once on this server, as root: echo \
             'net.ipv4.ip_unprivileged_port_start={port}' | sudo tee /etc/sysctl.d/60-vibeos.conf \
             && sudo sysctl --system"
        ),
        std::io::ErrorKind::AddrInUse => format!(
            "can't listen on port {port}: another program on this server has it, such as a web \
             server. https://{domain}/mcp needs it"
        ),
        _ => format!("can't listen on port {port}: {e}"),
    })
}

/// A listener on every IPv6 and IPv4 address at `port`, whatever the
/// system's default for IPv6 sockets (`bindv6only`) is.
fn dual_stack(port: u16) -> std::io::Result<TcpListener> {
    use std::os::fd::AsRawFd;
    let socket = tokio::net::TcpSocket::new_v6()?;
    let off: libc::c_int = 0;
    // SAFETY: setsockopt reads `off`, an int, from a pointer valid for the
    // call, on a socket this function owns.
    let r = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            (&off as *const libc::c_int).cast(),
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    socket.set_reuseaddr(true)?;
    socket.bind((std::net::Ipv6Addr::UNSPECIFIED, port).into())?;
    socket.listen(1024)
}

/// Sets `detail`, and logs it when it changes.
fn set_detail(detail: &Mutex<Option<String>>, now: Option<String>) {
    let mut d = lock(detail);
    if *d != now {
        match &now {
            Some(why) => crate::log!("vibed: external address: {why}"),
            None if d.is_some() => crate::log!("vibed: external address: working again"),
            None => {}
        }
        *d = now;
    }
}

/// Serves the public endpoint for `domain` until aborted: binds the port
/// (again and again until it can), keeps a certificate, and looks for the
/// Mac at the tunnel's end.
async fn run_public(
    core: Weak<Core>,
    domain: String,
    config: ExternalConfig,
    detail: Arc<Mutex<Option<String>>>,
) {
    let Some(c) = core.upgrade() else { return };
    let certs = Arc::new(Certs::new(&domain));
    let disk = Disk::new(c.layout.data_dir().join("external"));
    drop(c);
    match &config.fixed_cert {
        Some((chain, key)) => {
            let loaded = std::fs::read(chain)
                .and_then(|ch| Ok((ch, std::fs::read(key)?)))
                .map_err(|e| e.to_string())
                .and_then(|(ch, k)| acme::load_pem(&ch, &k));
            match loaded {
                Ok(l) => certs.set(l),
                Err(e) => set_detail(&detail, Some(format!("its certificate: {e}"))),
            }
        }
        None => match disk.load(&domain) {
            Some(Ok(l)) => certs.set(l),
            Some(Err(e)) => {
                crate::log!("vibed: the certificate kept for {domain}: {e}; getting another")
            }
            None => {}
        },
    }
    let listener = loop {
        match bind_public(config.listen, config.port, &domain).await {
            Ok(l) => break l,
            Err(why) => {
                set_detail(&detail, Some(why));
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    };
    let Some(c) = core.upgrade() else { return };
    crate::log!(
        "vibed: serving https://{domain}/mcp publicly, on port {}",
        config.port
    );
    let tls = vibe_mcp::public::server_config(Arc::clone(&certs) as _);
    let limiter = Arc::new(Limiter::new(config.limits));
    let _server = AbortOnDrop(tokio::spawn(vibe_mcp::public::serve(
        listener,
        tls,
        Arc::clone(&c),
        limiter,
    )));
    let _probe = AbortOnDrop(tokio::spawn(probe_mac(core.clone(), config.probe_every)));
    drop(c);
    if config.fixed_cert.is_some() {
        if certs.current().is_some() {
            set_detail(&detail, None);
        }
        std::future::pending::<()>().await;
    }
    // Let's Encrypt allows 5 failed checks an hour for a name: the first
    // retries are 5, 15 and 45 minutes apart. Turning it off and on again
    // tries at once, as after fixing DNS.
    let first_retry = Duration::from_secs(5 * 60);
    let mut backoff = first_retry;
    loop {
        let now = vibe_core::time::now();
        let current = certs.current().map(|l| l.validity);
        if let Some(v) = current
            && v.renew_at() > now
        {
            set_detail(&detail, None);
            let wait = u64::try_from(v.renew_at() - now).unwrap_or(0);
            tokio::time::sleep(Duration::from_secs(wait.min(12 * 3600))).await;
            continue;
        }
        let serving = current.is_some_and(|v| v.not_after > now);
        if !serving {
            set_detail(
                &detail,
                Some(format!(
                    "getting a certificate for {domain} from Let's Encrypt"
                )),
            );
        }
        crate::log!("vibed: asking Let's Encrypt for a certificate for {domain}");
        match acme::obtain(&config.acme_directory, &disk, &certs).await {
            Ok(v) => {
                crate::log!(
                    "vibed: got a certificate for {domain}, good until {}",
                    vibe_core::time::rfc3339(v.not_after)
                );
                set_detail(&detail, None);
                backoff = first_retry;
                // Never another order straight after one, whatever the
                // clock or the certificate's life says.
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            Err(why) => {
                crate::log!("vibed: no certificate for {domain} yet: {why}");
                if !serving {
                    set_detail(&detail, Some(why));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 3).min(Duration::from_secs(6 * 3600));
            }
        }
    }
}

/// Looks for the Mac at the tunnel's end every `every`, and says when it
/// comes or goes.
async fn probe_mac(core: Weak<Core>, every: Duration) {
    let mut was = None;
    loop {
        let Some(c) = core.upgrade() else { return };
        let up = c.peer.probe().await;
        let look_again = !up && c.peer.misses.load(Ordering::SeqCst) < GONE_AFTER;
        if was != Some(up) {
            if was.is_some() || up {
                crate::log!(
                    "vibed: the Mac is {} through its tunnel",
                    if up {
                        "reachable"
                    } else {
                        "no longer reachable"
                    }
                );
            }
            let mut e = c.event(EventKind::HostLink);
            e.state = Some(if up { "up" } else { "down" }.into());
            e.message = Some(format!(
                "the Mac's tunnel is {}",
                if up { "up" } else { "down" }
            ));
            c.publish(e);
            was = Some(up);
        }
        drop(c);
        // A first miss is looked into at once: calls under way are waiting on it.
        if !look_again {
            tokio::time::sleep(every).await;
        }
    }
}

impl Core {
    /// The public name this Linux host is the external address at, if it is.
    pub(crate) fn public_domain(&self) -> Option<String> {
        if !self.external.serve_here {
            return None;
        }
        self.store().host_external(LOCAL_HOST).ok().flatten()
    }

    /// The Mac behind this host, while it's the external address.
    pub(crate) fn mac(&self) -> Option<&MacPeer> {
        self.public_domain().map(|_| &self.peer)
    }

    /// Fills in what this host's own row says of external access: why it
    /// isn't working, if it isn't, and whether a Mac answers through the
    /// tunnel.
    pub(crate) fn describe_public(&self, info: &mut HostInfo) {
        if !info.external {
            return;
        }
        let public = lock(&self.public);
        info.external_detail = public.as_ref().and_then(|p| lock(&p.detail).clone());
        info.tunnel = Some(if self.peer.is_up() {
            TunnelState::Up
        } else {
            TunnelState::Down
        });
    }

    /// Serves the public endpoint while the registry says this host is the
    /// external address, and stops it once it isn't. Nothing until this
    /// core runs its background tasks (see [`Core::spawn_external`]).
    pub(crate) fn sync_public(&self) {
        let want = self.public_domain();
        let mut public = lock(&self.public);
        if public.as_ref().map(|p| &p.domain) == want.as_ref() {
            return;
        }
        if let Some(old) = public.take() {
            crate::log!("vibed: no longer serving https://{}/mcp", old.domain);
        }
        self.peer.reset();
        let (Some(domain), Some(me), Some(rt)) = (want, self.me.get(), self.runtime.get()) else {
            return;
        };
        let detail = Arc::new(Mutex::new(None));
        let task = rt.spawn(run_public(
            me.clone(),
            domain.clone(),
            self.external.clone(),
            Arc::clone(&detail),
        ));
        *public = Some(Public {
            domain,
            task,
            detail,
        });
    }

    /// Starts external access as the registry has it: on a Linux host, the
    /// public endpoint; on a Mac, the tunnel. It must be called on the
    /// runtime they're to run on.
    pub fn spawn_external(self: &Arc<Self>) {
        let _ = self.runtime.set(tokio::runtime::Handle::current());
        let _ = self.me.set(Arc::downgrade(self));
        self.sync_public();
        self.sync_tunnel();
    }

    /// Stops external access's tasks, as `vibed` does when it stops.
    pub fn stop_external(&self) {
        lock(&self.public).take();
        lock(&self.tunnel).take();
        self.close_tunnel_socket();
    }

    // tunnel.prepare

    /// Where the Mac's tunnel is to end, with what an earlier tunnel left
    /// there cleared: `ssh -R` won't bind over it, and `sshd` doesn't
    /// remove it when a tunnel closes. Any connection still on the old one
    /// goes on until it ends.
    pub fn prepare_tunnel(&self) -> Result<TunnelPrepared, ErrorObject> {
        let path = self.peer.socket.clone();
        // Its directory is private, so no other user on the server can
        // reach the socket sshd makes in it.
        if let Some(dir) = path.parent() {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)
                .and_then(|()| vibe_core::paths::private_dir(dir))
                .map_err(|e| {
                    error(
                        ErrorCode::InternalError,
                        format!("can't make {} private: {e}", dir.display()),
                    )
                })?;
        }
        match std::fs::symlink_metadata(&path) {
            Ok(m) => {
                use std::os::unix::fs::FileTypeExt;
                if !m.file_type().is_socket() {
                    return Err(error(
                        ErrorCode::Conflict,
                        format!("{} is there and isn't a socket", path.display()),
                    ));
                }
                std::fs::remove_file(&path).map_err(|e| {
                    error(
                        ErrorCode::InternalError,
                        format!("can't clear {}: {e}", path.display()),
                    )
                })?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(error(
                    ErrorCode::InternalError,
                    format!("can't look at {}: {e}", path.display()),
                ));
            }
        }
        lock(&self.peer.idle).clear();
        let socket = path
            .to_str()
            .filter(|s| !s.contains([':', '\n']))
            .ok_or_else(|| {
                error(
                    ErrorCode::InternalError,
                    format!(
                        "{} can't end an ssh tunnel: its path has a colon",
                        path.display()
                    ),
                )
            })?;
        Ok(TunnelPrepared {
            socket: socket.to_owned(),
        })
    }

    // hosts.external

    /// `hosts.external`: on a Mac, makes a Linux host the external address
    /// (or not); on a Linux host, serves it here (or not).
    pub async fn set_external(&self, p: HostExternalParams) -> Result<HostInfo, ErrorObject> {
        let host = p.host.as_deref().unwrap_or(LOCAL_HOST);
        let domain = if p.on {
            let d = p
                .domain
                .as_deref()
                .map(|d| d.trim().trim_end_matches('.').to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .ok_or_else(|| {
                    invalid("give the public name it's reached at, such as vps.example.com")
                })?;
            validate_domain(&d).map_err(invalid)?;
            Some(d)
        } else {
            None
        };
        if host == LOCAL_HOST {
            return self.set_external_here(domain);
        }
        let link = self.linux_host(host)?;
        // The host serves it, so it's told first while it can be: a refusal
        // there changes nothing here. While its link is down, it's told once
        // the link is back.
        if link.connected() {
            let theirs = self
                .forward_typed::<HostsExternal>(
                    &link,
                    &HostExternalParams {
                        host: None,
                        on: p.on,
                        domain: domain.clone(),
                    },
                )
                .await?;
            // What it says of itself now, without waiting for the next ping.
            link.note_report(theirs);
        }
        let others: Vec<String> = if domain.is_some() {
            self.store()
                .hosts()
                .map_err(store_error)?
                .into_iter()
                .filter(|h| h.external && h.name != host)
                .map(|h| h.name)
                .collect()
        } else {
            Vec::new()
        };
        self.store()
            .set_host_external(host, domain.as_deref())
            .map_err(store_error)?;
        match &domain {
            Some(d) => crate::log!("vibed: {host} is the external address, at https://{d}/mcp"),
            None => crate::log!("vibed: {host} is no longer the external address"),
        }
        // There's one external address: the one before it stops serving.
        for other in others {
            if let Some(l) = self.link(&other) {
                self.tell_external(&l, None);
                self.host_link_changed(&l);
            }
        }
        self.sync_tunnel();
        self.host_link_changed(&link);
        self.host_info(host)
    }

    fn set_external_here(&self, domain: Option<String>) -> Result<HostInfo, ErrorObject> {
        if !self.external.serve_here {
            return Err(invalid(
                "this Mac never listens publicly: make a Linux host with a public address the external address",
            ));
        }
        self.store()
            .set_host_external(LOCAL_HOST, domain.as_deref())
            .map_err(store_error)?;
        match &domain {
            Some(d) => crate::log!("vibed: this host is the external address, at https://{d}/mcp"),
            None => crate::log!("vibed: this host is no longer the external address"),
        }
        self.sync_public();
        let mut e = self.event(EventKind::HostLink);
        e.message = Some(match &domain {
            Some(d) => format!("the external address, at https://{d}/mcp"),
            None => "no longer the external address".into(),
        });
        self.publish(e);
        self.host_info(LOCAL_HOST)
    }

    /// Tells `link`'s host, in the background, whether it's the external
    /// address and at what name, as this Mac has it.
    pub(crate) fn tell_external(&self, link: &Arc<crate::link::Link>, domain: Option<String>) {
        let link = Arc::clone(link);
        self.in_background(|core| async move {
            let params = HostExternalParams {
                host: None,
                on: domain.is_some(),
                domain,
            };
            if let Err(e) = core.forward_typed::<HostsExternal>(&link, &params).await {
                crate::log!(
                    "vibed: can't tell {} whether it's the external address: {}",
                    link.name,
                    e.message
                );
            }
        });
    }

    /// Once `link` is up: if its host doesn't serve the external address as
    /// this Mac has it, it's told to. This Mac's choice wins.
    pub(crate) fn reconcile_external(&self, link: &Arc<crate::link::Link>) {
        let want = self.store().host_external(&link.name).ok().flatten();
        let theirs = link.reported_domain();
        if want != theirs {
            crate::log!(
                "vibed: {} {}; telling it",
                link.name,
                match &want {
                    Some(d) => format!("isn't serving https://{d}/mcp yet"),
                    None => "still serves an external address this Mac turned off".into(),
                }
            );
            self.tell_external(link, want);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macs_answers_are_its_own() {
        let mut v = json!({"agents": [
            {"host": "local", "ref": "quiet-amber-otter", "status": {"activity": "done"}},
            {"host": "vps", "ref": "x"},
        ], "host": "local"});
        relabel(&mut v);
        assert_eq!(
            v,
            json!({"agents": [
                {"host": "mac", "ref": "quiet-amber-otter", "status": {"activity": "done"}},
                {"host": "mac", "ref": "x"},
            ], "host": "mac"})
        );
    }

    /// A tunnel's end that takes connections and says nothing, as a sleeping Mac's does.
    fn silent_end(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("silent.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        path
    }

    const LONG: Duration = Duration::from_secs(3600);

    /// `method` on `peer`, under way, ending only when the peer gives up on it.
    fn under_way(
        peer: &Arc<MacPeer>,
        method: &'static str,
    ) -> tokio::task::JoinHandle<ErrorObject> {
        let peer = Arc::clone(peer);
        tokio::spawn(async move {
            peer.call("vk_x", method, json!({}), LONG)
                .await
                .unwrap_err()
        })
    }

    /// A tunnel's end that answers a hello saying what it does, and a call after
    /// `taking`, having said it has it if it does that at all.
    fn answering_end(dir: &std::path::Path, accepts: bool, taking: Duration) -> PathBuf {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let path = dir.join("answering.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    while stream.read_line(&mut line).await.unwrap_or(0) > 0 {
                        let call: Value = serde_json::from_str(&line).unwrap();
                        line.clear();
                        let say = |v: Value| format!("{v}\n").into_bytes();
                        let result = if call["method"] == "tunnel.call" {
                            if accepts {
                                let frame = json!({"jsonrpc": "2.0",
                                    "method": vibe_core::rpc::CALL_ACCEPTED,
                                    "params": {"id": call["id"]}});
                                let bytes = say(frame);
                                stream.get_mut().write_all(&bytes).await.unwrap();
                            }
                            tokio::time::sleep(taking).await;
                            json!({"done": true})
                        } else if accepts {
                            json!({"daemon": "vibed", (ACCEPTS_CALLS): true})
                        } else {
                            json!({"daemon": "vibed"})
                        };
                        let bytes =
                            say(json!({"jsonrpc": "2.0", "id": call["id"], "result": result}));
                        stream.get_mut().write_all(&bytes).await.unwrap();
                    }
                });
            }
        });
        path
    }

    #[tokio::test]
    async fn a_mac_that_takes_a_connection_and_says_nothing_fails_at_the_accept_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let peer = MacPeer::new(silent_end(dir.path()));
        peer.note_look(true);
        peer.accepts_calls.store(true, Ordering::SeqCst);
        for (method, check) in [
            ("agents.start", "check vibe_list_agents"),
            ("agents.delete", "check vibe_list_agents"),
            ("agents.send", "check vibe_read_messages"),
        ] {
            let begun = Instant::now();
            // Its own timeout is an hour, and no look at the tunnel has failed.
            let e = peer
                .call("vk_x", method, json!({}), LONG)
                .await
                .unwrap_err();
            assert!(begun.elapsed() < ACCEPT_WITHIN + Duration::from_secs(3));
            assert_eq!(e.code, ErrorCode::HostUnavailable.code());
            assert!(e.is_outcome_unknown(), "{}", e.message);
            assert!(
                e.message.contains(&format!("didn't take {method} in 5 s")),
                "{}",
                e.message
            );
            assert!(e.message.contains(check), "{}", e.message);
        }
        // A read: nothing unknown of it, just nobody there.
        let e = peer
            .call("vk_x", "agents.output", json!({}), LONG)
            .await
            .unwrap_err();
        assert!(!e.is_outcome_unknown());
        assert!(
            e.message.contains("didn't take agents.output"),
            "{}",
            e.message
        );
        assert!(peer.is_up(), "only a look says it's gone");
        assert_eq!(peer.calls.available_permits(), MAX_CALLS);
    }

    #[tokio::test]
    async fn a_call_the_mac_has_taken_may_run_past_the_accept_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let taking = ACCEPT_WITHIN + Duration::from_secs(1);
        let peer = MacPeer::new(answering_end(dir.path(), true, taking));
        assert!(peer.probe().await);
        assert!(peer.accepts_calls(), "its hello says so");
        let answer = peer.call("vk_x", "agents.start", json!({}), LONG).await;
        assert_eq!(answer.unwrap()["done"], true);
    }

    #[tokio::test]
    async fn an_older_mac_with_no_accepted_isnt_held_to_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let taking = ACCEPT_WITHIN + Duration::from_secs(1);
        let peer = MacPeer::new(answering_end(dir.path(), false, taking));
        assert!(peer.probe().await);
        assert!(!peer.accepts_calls(), "its hello doesn't say so");
        let answer = peer.call("vk_x", "agents.start", json!({}), LONG).await;
        assert_eq!(answer.unwrap()["done"], true);
    }

    #[test]
    fn a_ref_seen_answering_is_known_until_its_gone_or_a_listing_says_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let peer = MacPeer::new(dir.path().join(TUNNEL_END));
        assert!(!peer.knows_ref("vk_a", "new"));
        peer.note_ref("vk_a", "new");
        assert!(peer.knows_ref("vk_a", "new"));
        assert!(
            !peer.knows_ref("vk_b", "new"),
            "another key knows nothing of it"
        );

        // A listing is all of them.
        peer.saw("vk_a", &[]);
        assert!(!peer.knows_ref("vk_a", "new"));

        peer.note_ref("vk_a", "new");
        peer.forget_ref("vk_a", "new");
        assert!(!peer.knows_ref("vk_a", "new"));

        peer.note_ref("vk_a", "new");
        peer.forget("vk_a");
        assert!(!peer.knows_ref("vk_a", "new"));
        peer.note_ref("vk_a", "new");
        peer.forget_all();
        assert!(!peer.knows_ref("vk_a", "new"));
    }

    #[tokio::test]
    async fn one_miss_stops_new_calls_and_two_stop_those_under_way() {
        let dir = tempfile::tempdir().unwrap();
        let peer = Arc::new(MacPeer::new(silent_end(dir.path())));
        peer.note_look(true);
        let send = under_way(&peer, "agents.send");
        let start = under_way(&peer, "agents.start");
        let delete = under_way(&peer, "agents.delete");
        let read = under_way(&peer, "agents.list");
        tokio::time::sleep(Duration::from_millis(200)).await;

        // One miss: anything new fails at once, and it's known not to have gone.
        peer.note_look(false);
        let e = peer
            .call("vk_x", "agents.send", json!({}), LONG)
            .await
            .unwrap_err();
        assert!(e.message.contains("nothing was sent"), "{}", e.message);
        assert!(!e.is_outcome_unknown());
        // Those under way carry on: it may have been a host stalling for a moment.
        tokio::time::sleep(STILL_THERE_EVERY * 3).await;
        for call in [&send, &start, &delete, &read] {
            assert!(!call.is_finished());
        }

        // Two: they stop, nobody knowing how the changes ended, and their turns are free.
        peer.note_look(false);
        for (call, check) in [
            (send, "check vibe_read_messages before resending"),
            (start, "check vibe_list_agents before trying again"),
            (delete, "check vibe_list_agents before trying again"),
        ] {
            let e = tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(e.code, ErrorCode::HostUnavailable.code());
            assert!(e.is_outcome_unknown(), "{}", e.message);
            assert!(e.message.contains("Outcome unknown"), "{}", e.message);
            assert!(e.message.contains(check), "{}", e.message);
        }
        let e = read.await.unwrap();
        assert!(!e.is_outcome_unknown(), "nothing to know of a read");
        assert!(e.message.contains("stopped answering"), "{}", e.message);
        assert_eq!(peer.calls.available_permits(), MAX_CALLS);

        // An answer to a look puts it all right.
        peer.note_look(true);
        let e = peer
            .call("vk_x", "agents.list", json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(e.message.contains("didn't answer"), "{}", e.message);
    }

    #[tokio::test]
    async fn stuck_calls_dont_hold_up_a_ninth_for_long_or_outlast_the_looks() {
        let dir = tempfile::tempdir().unwrap();
        let peer = Arc::new(MacPeer::new(silent_end(dir.path())));
        peer.note_look(true);
        let stuck: Vec<_> = (0..MAX_CALLS)
            .map(|_| under_way(&peer, "agents.start"))
            .collect();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(peer.calls.available_permits(), 0);

        let begun = Instant::now();
        let e = peer
            .call("vk_x", "agents.send", json!({}), LONG)
            .await
            .unwrap_err();
        assert!(begun.elapsed() < TURN_WITHIN + Duration::from_secs(2));
        assert!(e.message.contains("too many calls"), "{}", e.message);
        assert!(e.message.contains("nothing was sent"), "{}", e.message);
        assert!(!e.is_outcome_unknown());

        // The Mac's found gone: each stuck call ends, and gives its turn back.
        peer.note_look(false);
        peer.note_look(false);
        for call in stuck {
            let e = tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap();
            assert!(
                e.message.contains("check vibe_list_agents"),
                "{}",
                e.message
            );
        }
        peer.note_look(true);
        let e = peer
            .call("vk_x", "agents.list", json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(!e.message.contains("too many calls"), "{}", e.message);
    }

    /// A tunnel's end that answers reads and closes each connection after one, and
    /// closes on a send without a word: it may have happened, and who's to say.
    fn closing_end(dir: &std::path::Path) -> (PathBuf, Arc<Mutex<Vec<String>>>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let path = dir.join("closing.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let heard = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&heard);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut stream = BufReader::new(stream);
                    let mut line = String::new();
                    if stream.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    let call: Value = serde_json::from_str(&line).unwrap();
                    let method = call["params"]["method"].as_str().unwrap().to_owned();
                    lock(&log).push(method.clone());
                    if method != "agents.send" {
                        let reply = json!({"jsonrpc": "2.0", "id": call["id"], "result": {}});
                        let _ = stream
                            .get_mut()
                            .write_all(format!("{reply}\n").as_bytes())
                            .await;
                    }
                });
            }
        });
        (path, heard)
    }

    #[tokio::test]
    async fn a_send_that_went_out_isnt_sent_again_and_only_a_look_says_the_mac_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let (end, heard) = closing_end(dir.path());
        let peer = MacPeer::new(end);
        peer.note_look(true);
        let quick = Duration::from_secs(5);

        // A read on a channel kept from before, now closed, just goes again.
        for _ in 0..2 {
            peer.call("vk_x", "agents.list", json!({}), quick)
                .await
                .unwrap();
        }
        assert_eq!(*lock(&heard), ["agents.list", "agents.list"]);

        // A send isn't risked on the kept one, and once out it's never repeated.
        let e = peer
            .call("vk_x", "agents.send", json!({}), quick)
            .await
            .unwrap_err();
        assert!(e.is_outcome_unknown(), "{}", e.message);
        assert!(
            e.message.contains("check vibe_read_messages"),
            "{}",
            e.message
        );
        assert_eq!(
            lock(&heard).iter().filter(|m| *m == "agents.send").count(),
            1
        );
        assert!(peer.is_up(), "one failed call says nothing of the others");

        // Its tunnel's end gone: that didn't go out, and nothing's unknown of it.
        let peer = MacPeer::new(dir.path().join("nothing-here.sock"));
        peer.note_look(true);
        let e = peer
            .call("vk_x", "agents.send", json!({}), quick)
            .await
            .unwrap_err();
        assert!(e.message.contains("nothing was sent"), "{}", e.message);
        assert!(!e.is_outcome_unknown());
        assert!(peer.is_up());
    }

    #[tokio::test]
    async fn a_look_that_fails_lets_go_of_kept_channels() {
        let dir = tempfile::tempdir().unwrap();
        let (end, _heard) = closing_end(dir.path());
        let peer = MacPeer::new(end);
        peer.note_look(true);
        peer.call("vk_x", "agents.list", json!({}), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(lock(&peer.idle).len(), 1);
        peer.note_look(false);
        assert!(lock(&peer.idle).is_empty());
    }

    #[tokio::test]
    async fn a_tunnel_down_is_unavailable_and_seen_lists_stay_with_their_key() {
        let dir = tempfile::tempdir().unwrap();
        let peer = MacPeer::new(dir.path().join(TUNNEL_END));
        let e = peer
            .call("vk_x", "agents.list", json!({}), Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::HostUnavailable.code());
        assert!(e.message.starts_with("mac is unavailable"), "{}", e.message);
        assert!(!peer.probe().await);
        assert!(peer.last_seen("vk_a").is_none());
        peer.saw("vk_a", &[]);
        assert_eq!(peer.last_seen("vk_a"), Some(vec![]));
        assert!(peer.last_seen("vk_b").is_none(), "another key sees nothing");
        peer.reset();
        assert!(peer.last_seen("vk_a").is_none());
    }
}
