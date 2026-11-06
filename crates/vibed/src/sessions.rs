//! Sessions: the terminals agents run in, each held by a `vibe-shim` that
//! serves `run/<ref>.sock` (PRD: Architecture). Shims are detached from
//! `vibed`, so the shims themselves are the record of what's running: a
//! restarted `vibed` finds every one again by scanning `run/` and asking it,
//! and a shim whose command exited leaves `session.json` in its agent's
//! directory.

use std::ffi::OsString;
use std::fs::{self, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use vibe_core::model::{SessionState, is_reserved_ref, validate_ref};
use vibe_core::paths::Layout;
use vibe_core::rpc::{SessionExit, SessionInfo};
use vibe_core::shim::{
    self, Frame, FrameKind, HEADER_LEN, Record, Refused, Reply, Request, Size, Status,
};
use vibe_core::socket;

/// How long a shim may take to answer. A shim's loop never blocks, so one
/// that takes longer is stuck.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a shim may take to start its command.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// What a session's socket says about it.
#[derive(Debug)]
pub enum Probe {
    /// A shim answered.
    Running(Status),
    /// Nothing serves the socket, or there's none.
    Gone,
    /// Something answered, but not as a working shim does.
    Broken(String),
}

/// Asks the shim at `socket` for its status. When it doesn't answer, what
/// failed doesn't decide whether it's gone, as the errors differ by OS
/// (macOS reports a socket nobody serves differently to a non-blocking
/// connect than Linux does). Instead: a shim removes its socket as its
/// command exits, and holds the socket's lock for its whole life, so with
/// the socket or the lock gone, so is the shim.
pub async fn probe(socket: &Path) -> Probe {
    // With no lock beside it, it was never a shim's (sshd's end of a
    // tunnel, say): not asked, as it wouldn't answer.
    if !socket::lock_path(socket).exists() {
        return Probe::Gone;
    }
    let why = match tokio::time::timeout(PROBE_TIMEOUT, ask_status(socket)).await {
        Ok(Ok(status)) => return Probe::Running(status),
        Ok(Err(e)) => e.to_string(),
        Err(_) => format!("no answer in {}s", PROBE_TIMEOUT.as_secs()),
    };
    if !socket.exists() || !socket::lock_held(socket) {
        Probe::Gone
    } else {
        Probe::Broken(why)
    }
}

/// A control connection to the shim at `socket`, welcomed.
async fn control(socket: &Path) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket).await?;
    let hello = shim::control_hello("vibed");
    stream
        .write_all(&Frame::json(FrameKind::Hello, &hello).encode())
        .await?;
    let welcome = read_frame(&mut stream).await?;
    match welcome.kind {
        FrameKind::Welcome => Ok(stream),
        FrameKind::Refused => {
            let refused: Refused = welcome.decode()?;
            Err(io::Error::other(format!(
                "the shim refused vibed: {}",
                refused.message
            )))
        }
        other => Err(unexpected(other)),
    }
}

async fn ask_status(socket: &Path) -> io::Result<Status> {
    let mut stream = control(socket).await?;
    stream
        .write_all(&Frame::json(FrameKind::Request, &Request::Status).encode())
        .await?;
    let reply = read_frame(&mut stream).await?;
    if reply.kind != FrameKind::Reply {
        return Err(unexpected(reply.kind));
    }
    match reply.decode::<Reply>()? {
        Reply::Status(status) => Ok(status),
        Reply::Error(e) => Err(io::Error::other(e)),
        other => Err(io::Error::other(format!("{other:?} instead of a status"))),
    }
}

/// Types `bytes` into the session at `socket`, as a control client: they
/// reach its command as if typed, and nobody counts as attached.
pub async fn send_input(socket: &Path, bytes: &[u8]) -> io::Result<()> {
    let send = async {
        let mut stream = control(socket).await?;
        stream
            .write_all(&Frame::new(FrameKind::Input, bytes.to_vec()).encode())
            .await?;
        // A status after the input: the shim answers in order, so once it
        // has, the input has reached the terminal's queue.
        stream
            .write_all(&Frame::json(FrameKind::Request, &Request::Status).encode())
            .await?;
        let reply = read_frame(&mut stream).await?;
        if reply.kind != FrameKind::Reply {
            return Err(unexpected(reply.kind));
        }
        Ok(())
    };
    tokio::time::timeout(PROBE_TIMEOUT, send)
        .await
        .unwrap_or_else(|_| {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("no answer in {}s", PROBE_TIMEOUT.as_secs()),
            ))
        })
}

fn unexpected(kind: FrameKind) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("the shim sent a {kind:?} frame"),
    )
}

async fn read_frame(stream: &mut UnixStream) -> io::Result<Frame> {
    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header).await?;
    let (kind, len) = shim::parse_header(header)?;
    let mut payload = vec![0; len];
    stream.read_exact(&mut payload).await?;
    Ok(Frame { kind, payload })
}

/// The refs that have a shim's socket in `run/`, sorted: every `<ref>.sock`
/// with its lock beside it, as every shim binds it, but `vibed`'s own.
///
/// A socket with no lock was never a shim's: sshd makes a Linux host's tunnel end,
/// `mac.sock`, and leaves none, so it's not taken for a dead session and removed
/// (issue #45). The ones that do have one, `vibed.sock` and the `tunnel.sock` an older
/// Mac's `vibed` left, are skipped by name, as any of
/// [`vibe_core::model::RESERVED_REFS`] is, unless `is_agent` says an agent has that
/// name: one made before the name was reserved.
pub fn socket_refs(run_dir: &Path, is_agent: impl Fn(&str) -> bool) -> Vec<String> {
    let Ok(entries) = fs::read_dir(run_dir) else {
        return Vec::new();
    };
    let mut refs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_socket()))
        .filter(|e| socket::lock_path(&e.path()).is_file())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let r = name.strip_suffix(".sock")?;
            validate_ref(r).is_ok().then(|| r.to_owned())
        })
        .filter(|r| !is_reserved_ref(r) || is_agent(r))
        .collect();
    refs.sort();
    refs
}

/// The refs whose agent directory has a session record, sorted.
pub fn record_refs(agents_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(agents_dir) else {
        return Vec::new();
    };
    let mut refs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.path().join(shim::RECORD_FILE).is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|r| validate_ref(r).is_ok())
        .collect();
    refs.sort();
    refs
}

/// The record the shim of `agent_ref` left when its command exited.
pub fn read_record(layout: &Layout, agent_ref: &str) -> Option<Record> {
    let path = layout.agent_dir(agent_ref).join(shim::RECORD_FILE);
    let text = fs::read_to_string(&path).ok()?;
    match serde_json::from_str(&text) {
        Ok(r) => Some(r),
        Err(e) => {
            crate::log!("vibed: ignoring {}: {e}", path.display());
            None
        }
    }
}

/// Removes a shim's socket nobody serves, unless a shim is starting on it:
/// one holds the socket's lock for its whole life. A socket with no lock
/// beside it was never a shim's (every shim binds through
/// [`vibe_core::socket::bind`], which leaves one), so it's kept: sshd's end
/// of a tunnel is such a one (issue #45). Returns whether it removed it.
pub fn remove_stale(socket_path: &Path) -> io::Result<bool> {
    let lock_path = socket::lock_path(socket_path);
    let lock = match OpenOptions::new().write(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(e)) => return Err(e),
    }
    match fs::symlink_metadata(socket_path) {
        Ok(meta) if meta.file_type().is_socket() => fs::remove_file(socket_path).map(|()| true),
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// What a session looks like to clients, from its shim's status.
pub fn running(host: &str, title: String, status: &Status) -> SessionInfo {
    SessionInfo {
        host: host.to_owned(),
        agent: status.agent.clone(),
        state: SessionState::Running,
        title,
        clients: status.clients.len() as u32,
        ttys: ttys(status),
        exit: None,
    }
}

pub fn ttys(status: &Status) -> Vec<String> {
    status
        .clients
        .iter()
        .filter_map(|c| c.tty.clone())
        .collect()
}

pub fn exited(host: &str, agent_ref: &str, title: String, exit: SessionExit) -> SessionInfo {
    SessionInfo {
        host: host.to_owned(),
        agent: agent_ref.to_owned(),
        state: SessionState::Exited,
        title,
        clients: 0,
        ttys: Vec::new(),
        exit: Some(exit),
    }
}

/// How an exit reads in a sentence: "exited with code 3".
pub fn describe_exit(exit: &SessionExit) -> String {
    match (&exit.code, &exit.signal) {
        (Some(code), _) => format!("exited with code {code}"),
        (None, Some(signal)) => format!("was ended by {signal}"),
        (None, None) => "ended".to_owned(),
    }
}

/// What to start a session with.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub agent: String,
    pub command: Vec<OsString>,
    pub cwd: PathBuf,
    /// Set for the command on top of `vibed`'s own environment.
    pub env: Vec<(OsString, OsString)>,
    /// `env` is all it gets: the shim and the command start from nothing of
    /// `vibed`'s.
    pub clean_env: bool,
    /// The terminal's size until a client attaches.
    pub size: Option<Size>,
    /// For a `linux` agent on the Mac: the `vibe-vm` socket of its VM,
    /// whose `vibe-guest` already runs the command on a terminal of its
    /// own. Then `command`, `cwd` and `env` aren't used.
    pub vm: Option<PathBuf>,
}

/// Runs `vibe-shim` for `spec` and waits until its command is running.
/// The shim detaches, so it outlives `vibed`. Returns its pid, or what it
/// said went wrong.
pub async fn spawn_shim(
    shim_bin: &OsString,
    layout: &Layout,
    spec: &SessionSpec,
) -> Result<u32, String> {
    let mut cmd = tokio::process::Command::new(shim_bin);
    cmd.arg("--ref")
        .arg(&spec.agent)
        .arg("--socket")
        .arg(layout.session_socket(&spec.agent))
        .arg("--state")
        .arg(layout.agent_dir(&spec.agent));
    if let Some(size) = spec.size {
        cmd.arg("--size").arg(size.to_string());
    }
    match &spec.vm {
        Some(vm) => {
            cmd.arg("--vm").arg(vm);
        }
        None => {
            cmd.arg("--cwd")
                .arg(&spec.cwd)
                .arg("--")
                .args(&spec.command);
            if spec.clean_env {
                cmd.env_clear();
            } else {
                vibe_core::keychain::withhold_allow(cmd.as_std_mut());
            }
            cmd.envs(spec.env.iter().map(|(k, v)| (k, v)));
        }
    }
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(START_TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            return Err(format!("can't run {}: {e}", Path::new(shim_bin).display()));
        }
        Err(_) => {
            return Err(format!(
                "vibe-shim didn't start in {}s",
                START_TIMEOUT.as_secs()
            ));
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() {
        let said = stderr.trim();
        let said = said.strip_prefix("vibe-shim: ").unwrap_or(said);
        return Err(if said.is_empty() {
            format!("vibe-shim failed ({})", out.status)
        } else {
            said.to_owned()
        });
    }
    stdout
        .trim()
        .parse()
        .map_err(|_| format!("vibe-shim printed {stdout:?}, not its pid"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_come_from_sockets_and_records() {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        fs::create_dir(&run).unwrap();
        let socket = |name: &str, lock: bool| {
            drop(std::os::unix::net::UnixListener::bind(run.join(name)).unwrap());
            if lock {
                fs::write(socket::lock_path(&run.join(name)), "").unwrap();
            }
        };
        for name in ["vibed.sock", "b-ref.sock", "a-ref.sock", "not a ref.sock"] {
            socket(name, true);
        }
        // Every shim leaves a lock beside its socket: one without isn't a
        // shim's.
        socket("no-lock.sock", false);
        fs::write(run.join("plain.sock"), "not a socket").unwrap();
        fs::write(run.join("plain.sock.lock"), "").unwrap();
        let none = |_: &str| false;
        assert_eq!(socket_refs(&run, none), ["a-ref", "b-ref"]);
        assert!(socket_refs(&tmp.path().join("none"), none).is_empty());

        // What an older `vibed` may have left: a host's tunnel end, which sshd
        // leaves with no lock, and the Mac's, with one. Neither is a session, but
        // for an agent named so before the name was reserved.
        socket("mac.sock", false);
        socket("tunnel.sock", true);
        assert_eq!(socket_refs(&run, none), ["a-ref", "b-ref"]);
        assert_eq!(
            socket_refs(&run, |r| r == "tunnel" || r == "mac"),
            ["a-ref", "b-ref", "tunnel"]
        );

        let agents = tmp.path().join("agents");
        for r in ["x-ref", "y-ref", "no-record"] {
            fs::create_dir_all(agents.join(r)).unwrap();
        }
        fs::write(agents.join("y-ref").join(shim::RECORD_FILE), "{}").unwrap();
        fs::write(agents.join("x-ref").join(shim::RECORD_FILE), "{}").unwrap();
        assert_eq!(record_refs(&agents), ["x-ref", "y-ref"]);
    }

    #[test]
    fn the_daemons_sockets_have_reserved_names() {
        for name in [vibe_core::paths::SOCKET_NAME, "mac.sock", "tunnel.sock"] {
            let stem = name.strip_suffix(".sock").unwrap();
            assert!(vibe_core::model::RESERVED_REFS.contains(&stem), "{name}");
        }
    }

    #[test]
    fn stale_sockets_go_unless_a_shim_holds_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("q.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        // A shim starting up holds the lock.
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(socket::lock_path(&path))
            .unwrap();
        lock.lock().unwrap();
        assert!(!remove_stale(&path).unwrap());
        assert!(path.exists());
        drop(lock);
        // This process may hold the lock a moment longer: a child another test
        // is spawning shares the open file until its exec closes it, as
        // `StateLock::take` allows for.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !remove_stale(&path).unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "the lock was never let go"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!path.exists());
        assert!(!remove_stale(&path).unwrap(), "nothing left to remove");
        // Never anything but a socket.
        fs::write(&path, "keep").unwrap();
        assert!(!remove_stale(&path).unwrap());
        assert!(path.exists());

        // A socket with no lock beside it was never a shim's, as sshd's end
        // of a tunnel isn't (issue #45): it's kept.
        let tunnel_end = tmp.path().join("mac.sock");
        drop(std::os::unix::net::UnixListener::bind(&tunnel_end).unwrap());
        assert!(!remove_stale(&tunnel_end).unwrap());
        assert!(tunnel_end.exists());
    }

    #[test]
    fn exits_in_words() {
        let at = "2026-10-30T12:00:00Z".to_owned();
        let code = SessionExit {
            code: Some(3),
            signal: None,
            at: at.clone(),
        };
        assert_eq!(describe_exit(&code), "exited with code 3");
        let signal = SessionExit {
            code: None,
            signal: Some("SIGKILL".into()),
            at: at.clone(),
        };
        assert_eq!(describe_exit(&signal), "was ended by SIGKILL");
        let neither = SessionExit {
            code: None,
            signal: None,
            at,
        };
        assert_eq!(describe_exit(&neither), "ended");
    }

    #[tokio::test]
    async fn probes_tell_gone_from_broken() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            probe(&tmp.path().join("none.sock")).await,
            Probe::Gone
        ));
        // A socket nobody serves, with or without the lock file a dead
        // shim leaves.
        let stale = tmp.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
        assert!(matches!(probe(&stale).await, Probe::Gone));
        std::fs::write(socket::lock_path(&stale), "").unwrap();
        assert!(matches!(probe(&stale).await, Probe::Gone));

        // Something holding the lock but not answering as a shim does: alive,
        // so broken, not gone.
        let other = tmp.path().join("other.sock");
        let held = held_lock(&other);
        let listener = tokio::net::UnixListener::bind(&other).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = listener.accept().await.unwrap();
                let _ = s.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n").await;
            }
        });
        assert!(matches!(probe(&other).await, Probe::Broken(_)));
        // The same answer once its lock is free: whatever answered is no
        // shim of ours, and the shim that was is gone. A lock is let go when
        // the last descriptor of its open file closes, and a command another
        // test spawns meanwhile holds copies of all of this process's
        // descriptors until its exec closes them, which macOS's code-signing
        // checks can make take a while. So "free" is waited for, not
        // assumed the moment `held` drops.
        drop(held);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while socket::lock_held(&other) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the lock stayed held after it was let go"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let after = probe(&other).await;
        assert!(
            matches!(after, Probe::Gone),
            "{after:?} with its lock let go; lock_held says {}",
            socket::lock_held(&other)
        );

        // A stale socket whose lock a shim holds as it starts is left be.
        let starting = tmp.path().join("starting.sock");
        drop(std::os::unix::net::UnixListener::bind(&starting).unwrap());
        let _held = held_lock(&starting);
        assert!(matches!(probe(&starting).await, Probe::Broken(_)));
    }

    /// The lock beside `socket`, held as a running shim holds it.
    fn held_lock(socket: &Path) -> std::fs::File {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(socket::lock_path(socket))
            .unwrap();
        lock.lock().unwrap();
        lock
    }
}
