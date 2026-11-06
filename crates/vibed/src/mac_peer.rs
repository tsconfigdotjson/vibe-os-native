//! The Mac, from a Linux host (PRD: Reaching MCP from elsewhere): a key-authenticated MCP
//! on the host, on loopback or its Tailscale address, reaches agents on the Mac too.
//!
//! The Mac keeps an SSH reverse tunnel to a Unix socket in the host's private `run/` (see
//! [`crate::tunnel`]). Callers of the host's MCP see the Mac's agents as `mac:<ref>`; a
//! call for one goes down the tunnel with the caller's key, and the Mac checks the key
//! itself, so a host that's been taken over can't drive the Mac without one. While the
//! Mac can't be reached, the host answers for its own agents, and the Mac's are
//! unavailable, as they were last seen.
//!
//! Nothing here listens on the network. The tunnel's end exists only once a Mac has
//! opened one, so a Linux host with no Mac has no `mac` to speak of.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use vibe_core::model::{EventKind, LOCAL_HOST, MAC_HOST};
use vibe_core::rpc::{ACCEPTS_CALLS, MemoryUse};
use vibe_core::rpc::{Agent, ErrorCode, ErrorObject, Method, Project, TunnelPrepared};

use crate::core::{Core, error, lock};
use crate::link::check_after;
use crate::remote::{ACCEPT_WITHIN, Channel};

/// Where the Mac's tunnel ends on a Linux host, in its private `run/`.
pub const TUNNEL_END: &str = "mac.sock";

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
    /// The other hosts linked to the Mac, reached through it, as each key last heard.
    linked_seen: Mutex<HashMap<String, Vec<String>>>,
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

/// The Mac's answer, as this host says it: its own `local` is `mac`, and a host linked to
/// it goes by its own name.
fn relabel(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("host").and_then(Value::as_str) == Some(LOCAL_HOST) {
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
            linked_seen: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn is_up(&self) -> bool {
        self.up.load(Ordering::SeqCst)
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
        // `mac` is what it goes by here, not there; a host linked to it keeps its name.
        if let Some(map) = params.as_object_mut()
            && matches!(
                map.get("host").and_then(Value::as_str),
                None | Some(MAC_HOST | LOCAL_HOST)
            )
        {
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

    /// Notes the other hosts linked to the Mac, as `key` just heard.
    pub(crate) fn saw_linked(&self, key: &str, hosts: Vec<String>) {
        let mut seen = lock(&self.linked_seen);
        if seen.len() >= MAX_SEEN && !seen.contains_key(&vibe_core::keys::hash(key)) {
            seen.clear();
        }
        seen.insert(vibe_core::keys::hash(key), hosts);
    }

    /// The other hosts linked to the Mac, as `key` last heard, if it has.
    pub(crate) fn linked(&self, key: &str) -> Option<Vec<String>> {
        lock(&self.linked_seen)
            .get(&vibe_core::keys::hash(key))
            .cloned()
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
        lock(&self.linked_seen).clear();
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

/// Looks for the Mac at the tunnel's end every `every`, and says when it
/// comes or goes.
async fn probe_mac(core: Weak<Core>, every: Duration) {
    let mut was = None;
    loop {
        let Some(c) = core.upgrade() else { return };
        let up = c.peer.probe().await;
        let look_again = !up && c.peer.misses.load(Ordering::SeqCst) < GONE_AFTER;
        // A host no Mac has ever tunnelled to has nothing to say of it.
        if was != Some(up) && (was.is_some() || up) {
            crate::log!(
                "vibed: the Mac is {} through its tunnel",
                if up {
                    "reachable"
                } else {
                    "no longer reachable"
                }
            );
            let mut e = c.event(EventKind::HostLink);
            e.state = Some(if up { "up" } else { "down" }.into());
            e.message = Some(format!(
                "the Mac's tunnel is {}",
                if up { "up" } else { "down" }
            ));
            c.publish(e);
        }
        was = Some(up);
        drop(c);
        // A first miss is looked into at once: calls under way are waiting on it.
        if !look_again {
            tokio::time::sleep(every).await;
        }
    }
}

impl Core {
    /// The Mac behind this host, once one has opened a tunnel to it. Its end stays in
    /// `run/` when the Mac sleeps, as sshd leaves it, so the Mac's agents go on being
    /// listed, unavailable, until it's back or has forgotten this host.
    pub(crate) fn mac(&self) -> Option<&MacPeer> {
        (self.serves_mac_peer && std::fs::symlink_metadata(&self.peer.socket).is_ok())
            .then_some(&self.peer)
    }

    /// Looks at the tunnel's end from now on, on a host a Mac can tunnel to.
    pub(crate) fn spawn_mac_probe(&self, me: Weak<Core>, rt: &tokio::runtime::Handle) {
        if self.serves_mac_peer {
            let every = self.mac_probe_every;
            *lock(&self.mac_probe) = Some(rt.spawn(probe_mac(me, every)));
        }
    }

    /// Stops looking.
    pub(crate) fn stop_mac_probe(&self) {
        if let Some(task) = lock(&self.mac_probe).take() {
            task.abort();
        }
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_macs_own_answers_are_labelled_mac_and_a_linked_hosts_keep_their_name() {
        let mut v = json!({"agents": [
            {"host": "local", "ref": "quiet-amber-otter", "status": {"activity": "done"}},
            {"host": "vps", "ref": "x"},
        ], "host": "local"});
        relabel(&mut v);
        assert_eq!(
            v,
            json!({"agents": [
                {"host": "mac", "ref": "quiet-amber-otter", "status": {"activity": "done"}},
                {"host": "vps", "ref": "x"},
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
        peer.forget_all();
        assert!(peer.last_seen("vk_a").is_none());
    }
}
