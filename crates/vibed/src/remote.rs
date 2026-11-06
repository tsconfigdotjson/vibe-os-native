//! How this `vibed` reaches a Linux host (PRD: Adding a Linux host, The link
//! between the Mac and a host). Everything goes over SSH: the install, and
//! then `vibe rpc` on the server, which carries the same JSON-RPC as the
//! local socket. Nothing new is opened on the server, and SSH keys are the
//! authentication.
//!
//! [`Remote`] is the seam: [`Ssh`] is the real thing, and tests put other
//! carriers behind it.

use std::ffi::OsString;
use std::fmt;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
};
use vibe_core::model::AdminNeed;
use vibe_core::rpc::{CALL_ACCEPTED, ErrorObject, MAX_MESSAGE_BYTES, Outcome, Response};

/// A future a [`Remote`] returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where `vibed` and its two companions live on a Linux host, relative to
/// the home directory `ssh` starts commands in. `~/.local/bin` gets links
/// to `vibe` and `vibed` for the owner's own shells.
pub const HOST_LIB: &str = ".local/lib/vibeos";

/// The `vibe` this host runs over SSH. A command over SSH doesn't get a
/// login shell's `PATH`, so it's named by its path from the home directory.
pub const HOST_VIBE: &str = ".local/lib/vibeos/vibe";

/// The `vibe` an attach line names: VibeOS's own, not whatever
/// `~/.local/bin/vibe` is (the install leaves one that isn't VibeOS's).
pub const HOST_ATTACH_VIBE: &str = HOST_VIBE;

/// What an install puts on a Linux host, from the bundle for its CPU.
pub const BINARIES: [&str; 3] = ["vibed", "vibe", "vibe-shim"];

/// The tools a Linux host's agents need, which the owner installs and logs
/// in to on the server (PRD: Adding a Linux host).
pub const TOOLS: [&str; 3] = ["git", "gh", "claude"];

/// The longest line kept of what `ssh` said on stderr.
const WHY_CHARS: usize = 300;

/// What an install found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// `linux`.
    pub os: String,
    /// `x86_64` or `aarch64`.
    pub arch: String,
    /// The server's hostname.
    pub label: String,
    /// The `vibed` it runs now.
    pub version: String,
    /// What it lacks of [`TOOLS`].
    pub missing: Vec<String>,
    /// What it needs someone with `sudo` there to allow, as the install found.
    pub needs_admin: Vec<AdminNeed>,
    /// Whether anything on the server changed.
    pub changed: bool,
    /// What the owner should know, in sentences.
    pub notes: Vec<String>,
}

/// An SSH reverse tunnel, kept up until it ends or is dropped.
pub struct TunnelChild {
    /// Resolves, with why, once the tunnel has ended.
    pub ended: BoxFuture<'static, String>,
}

impl fmt::Debug for TunnelChild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TunnelChild").finish_non_exhaustive()
    }
}

/// How this `vibed` reaches Linux hosts.
pub trait Remote: Send + Sync + fmt::Debug {
    /// Opens a JSON-RPC connection to `vibed` on the host at `address`.
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>>;

    /// Installs or upgrades `vibed` on the host at `address`, and checks
    /// its tools.
    fn install<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Installed, String>>;

    /// Forwards the Unix socket `remote` on the host at `address` to the
    /// socket `local` here (`ssh -R remote:local`), until the tunnel ends or
    /// is dropped (the Mac dials out).
    fn tunnel<'a>(
        &'a self,
        address: &'a str,
        _remote: &'a str,
        _local: &'a std::path::Path,
    ) -> BoxFuture<'a, Result<TunnelChild, String>> {
        Box::pin(async move { Err(format!("can't keep a tunnel to {address}")) })
    }

    /// The same, for the link's own session, which may share a connection
    /// with the install steps.
    fn dial_link<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        self.dial(address)
    }

    /// Closes the connection `address`'s commands share, if there is one,
    /// once it has stopped answering: the next command makes a new one.
    fn drop_shared(&self, _address: &str) {}

    /// The command a terminal here runs to do what `needs` take on `address`, with
    /// `sudo` asking for its password there. What each takes is written here,
    /// never given by the host, and the password goes to `sudo` alone.
    fn admin_line(&self, address: &str, needs: &[AdminNeed]) -> String {
        let mut script = String::new();
        for need in needs {
            let step = match need {
                AdminNeed::Linger => "sudo loginctl enable-linger \"$(id -un)\"\n".to_owned(),
                AdminNeed::Git => crate::tool_install::GIT_WITH_SUDO.replace("{sudo}", "sudo"),
            };
            script.push_str(&format!(
                "{{\n{step}}} || {{\n  echo \"VibeOS: that didn't work, for the reason above. Try \
                 again from Settings when it's put right.\"\n  exit 1\n}}\n"
            ));
        }
        script.push_str("echo \"VibeOS: done. You can close this window.\"\n");
        // Encoded, and piped to `sh`, as the login shell there can be one that reads
        // quotes and newlines otherwise, such as csh or fish. `sudo` asks on the
        // terminal, not on this pipe.
        format!(
            "ssh -t {} {}",
            shell_quote(address),
            shell_quote(&format!(
                "echo {} | base64 -d | sh",
                base64(script.as_bytes())
            ))
        )
    }

    /// The command a terminal here runs to attach to `agent` on `address`
    /// (PRD: The link, "Clicking a remote agent opens Terminal with …").
    fn attach_line(&self, address: &str, agent: &str) -> String {
        // The menu runs it in a shell, and the ref is the host's word, so
        // it's quoted too: a valid ref reads as it is.
        format!(
            "ssh -t {} {HOST_ATTACH_VIBE} attach {}",
            shell_quote(address),
            shell_quote(agent)
        )
    }
}

/// `s` as one word for a POSIX shell: as it is if that's safe, otherwise in
/// single quotes.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(c, '.' | '_' | '-' | '@' | ':' | '/' | '%' | '+' | '=' | ',')
        });
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

// Channels.

/// A JSON-RPC connection to a host's `vibed`: one message per line each way,
/// as on the local socket. Dropping it ends whatever carries it.
pub struct Channel {
    reader: Box<dyn AsyncBufRead + Send + Unpin>,
    writer: Box<dyn AsyncWrite + Send + Unpin>,
    _carrier: Box<dyn Send + Sync>,
    why: Arc<Mutex<String>>,
    next_id: i64,
    line: Vec<u8>,
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Channel")
            .field("next_id", &self.next_id)
            .finish_non_exhaustive()
    }
}

impl Channel {
    /// A channel over `reader` and `writer`. `carrier` is kept until the
    /// channel is dropped (an `ssh` process, say), and `why` is where the
    /// carrier says why it ended.
    pub fn new(
        reader: impl AsyncBufRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        carrier: impl Send + Sync + 'static,
        why: Arc<Mutex<String>>,
    ) -> Channel {
        Channel {
            reader: Box::new(reader),
            writer: Box::new(writer),
            _carrier: Box::new(carrier),
            why,
            next_id: 1,
            line: Vec::new(),
        }
    }

    /// Why the carrier ended, as it said, if it said.
    pub fn why(&self) -> Option<String> {
        let why = self.why.lock().unwrap_or_else(|e| e.into_inner());
        (!why.is_empty()).then(|| why.clone())
    }

    fn closed(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            match self.why() {
                Some(why) => format!("the link closed: {why}"),
                None => "the link closed".into(),
            },
        )
    }

    /// Sends one message.
    pub async fn send(&mut self, message: &Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await
    }

    /// Reads the next message; `None` once the other side has closed.
    ///
    /// Cancel-safe, as the link reads in a `select!` beside its pings: a
    /// line cut off by a cancel stays in the buffer, and the next read
    /// carries on with it.
    pub async fn read(&mut self) -> io::Result<Option<Value>> {
        let limit = MAX_MESSAGE_BYTES as u64 + 1;
        let room = limit.saturating_sub(self.line.len() as u64);
        let n = (&mut self.reader)
            .take(room)
            .read_until(b'\n', &mut self.line)
            .await?;
        if self.line.last() != Some(&b'\n') {
            let too_large = self.line.len() as u64 >= limit;
            let empty = self.line.is_empty();
            self.line.clear();
            return if too_large {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a message is too large",
                ))
            } else if n == 0 && empty {
                Ok(None)
            } else {
                Err(self.closed())
            };
        }
        let parsed = serde_json::from_slice(&self.line);
        self.line.clear();
        parsed
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Sends a request with a new id and returns its id.
    pub async fn request(&mut self, method: &str, params: Value) -> io::Result<i64> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        Ok(id)
    }

    /// Calls `method` and waits for its response, passing over anything
    /// else that comes first, such as events. An error the host answers
    /// with is the inner `Err`; the outer one is the channel failing.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
    ) -> io::Result<Result<Value, ErrorObject>> {
        let id = self.request(method, params).await?;
        loop {
            let Some(message) = self.read().await? else {
                return Err(self.closed());
            };
            if message.get("id") == Some(&json!(id)) {
                return outcome(message);
            }
        }
    }

    /// [`Channel::call`] on a peer that sends [`CALL_ACCEPTED`]: it fails with
    /// [`io::ErrorKind::TimedOut`] if neither that nor the answer comes `within` this
    /// long, the bytes having gone nobody knows where.
    pub async fn call_accepted(
        &mut self,
        method: &str,
        params: Value,
        within: Duration,
    ) -> io::Result<Result<Value, ErrorObject>> {
        let id = self.request(method, params).await?;
        let deadline = tokio::time::Instant::now() + within;
        let mut accepted = false;
        loop {
            let message = if accepted {
                self.read().await?
            } else {
                match tokio::time::timeout_at(deadline, self.read()).await {
                    Ok(message) => message?,
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("it didn't take {method} in {} s", within.as_secs()),
                        ));
                    }
                }
            };
            let Some(message) = message else {
                return Err(self.closed());
            };
            if message.get("id") == Some(&json!(id)) {
                return outcome(message);
            }
            if message.get("method").and_then(Value::as_str) == Some(CALL_ACCEPTED)
                && message["params"]["id"] == json!(id)
            {
                accepted = true;
            }
        }
    }
}

/// How long a peer that sends [`CALL_ACCEPTED`] has to say it has a call. After that
/// it's the method's own timeout: a call may take as long as it takes.
pub(crate) const ACCEPT_WITHIN: Duration = Duration::from_secs(5);

/// A response's result or error.
pub fn outcome(message: Value) -> io::Result<Result<Value, ErrorObject>> {
    let response: Response = serde_json::from_value(message)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(match response.outcome {
        Outcome::Result(v) => Ok(v),
        Outcome::Error(e) => Err(e),
    })
}

/// Keeps the last line `stderr` writes in `why`, until it closes.
fn keep_last_line(
    stderr: impl tokio::io::AsyncRead + Send + Unpin + 'static,
) -> Arc<Mutex<String>> {
    let why = Arc::new(Mutex::new(String::new()));
    let keep = Arc::clone(&why);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if !line.is_empty() && !line.contains(NOT_SHARING) {
                *keep.lock().unwrap_or_else(|e| e.into_inner()) =
                    line.chars().take(WHY_CHARS).collect();
            }
        }
    });
    why
}

// SSH.

/// The options every `ssh` gets. Never a prompt: `vibed` has no terminal,
/// so a password or passphrase it can't get from the agent fails at once.
/// A server's first key is taken, as a first `ssh` by hand would ask to;
/// a changed one never is. Keepalives notice a pulled network within
/// half a minute, and the link pings on top of that. Nothing of the Mac's
/// goes to the server but the connection itself: not its SSH agent, nor X11,
/// whatever `~/.ssh/config` says, as the server may not deserve them.
const SSH_OPTIONS: [&str; 20] = [
    "-a",
    "-x",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "BatchMode=yes",
    "-o",
    "StrictHostKeyChecking=accept-new",
    "-o",
    "ServerAliveInterval=10",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ConnectTimeout=20",
    "-o",
    "LogLevel=ERROR",
];

/// How long a step of an install may take: an upload over a slow link can
/// take minutes.
const STEP_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// How long a host's shared connection stays up once nothing uses it.
const SHARED_FOR: Duration = Duration::from_secs(60);

/// The longest control socket path: a Unix socket's path is at most 103
/// bytes on macOS (107 on Linux), and `ssh` makes the socket under its name
/// and 17 more before moving it into place.
const MOST_CONTROL_PATH: usize = 103 - 17;

/// What `ssh` says when another of its commands made the shared
/// connection's socket first. It carries on with a connection of its own,
/// so that's no reason for anything.
const NOT_SHARING: &str = "disabling multiplexing";

/// The real thing: `ssh`, and the Linux builds of `vibed` this one carries.
#[derive(Debug, Clone)]
pub struct Ssh {
    /// The `ssh` to run.
    pub program: OsString,
    /// Where the Linux builds are: `<bundle>/<arch>/vibed`, `vibe` and
    /// `vibe-shim`, for `x86_64` and `aarch64`.
    pub bundle: Option<PathBuf>,
    /// The private directory for the sockets of the connections each host's
    /// commands share (see [`Ssh::sharing`]), or none for a connection per
    /// command.
    pub shared: Option<PathBuf>,
}

impl Ssh {
    /// `ssh` on `PATH`, and the bundle [`default_bundle`] finds. Each
    /// command gets its own connection until [`Ssh::sharing_in`] says where
    /// to keep shared ones.
    pub fn new() -> Ssh {
        let path = std::env::var_os("PATH");
        Ssh {
            program: crate::core::find_program("ssh", path.as_deref()),
            bundle: default_bundle(),
            shared: None,
        }
    }

    /// This, with each host's commands sharing a connection whose socket is
    /// in `dir` (the run directory).
    pub fn sharing_in(mut self, dir: PathBuf) -> Ssh {
        self.shared = Some(dir);
        self
    }

    /// The options that have `address`'s install steps and its link share
    /// one connection: the first starts it, the rest go through it, and it
    /// closes a minute after the last ends. An install is five commands,
    /// and the link follows at once; a server that limits how often a
    /// client may connect (ufw's `limit`, fail2ban, sshd's `MaxStartups`)
    /// would turn that many connections away (issue #39). The link's calls
    /// keep connections of their own, so the shared one carries two
    /// sessions at most, well inside sshd's `MaxSessions`.
    ///
    /// The socket goes in the run directory, or, when its path there would
    /// be too long for a socket or can't be written in an option (a long
    /// home directory), in `/tmp/vibeos-<uid>`. None, for a connection per
    /// command, when neither will do.
    pub fn sharing(&self, address: &str) -> Vec<String> {
        let Some(dir) = &self.shared else {
            return Vec::new();
        };
        let name = format!("ssh-{}", &sha256_hex(address.as_bytes())[..16]);
        let usable = |dir: &std::path::Path| {
            let path = dir.join(&name);
            let text = path.to_str()?.to_owned();
            // ssh expands % and ${…} in it, and unescapes \ in quotes.
            (text.len() <= MOST_CONTROL_PATH
                && !text.contains(['"', '%', '$', '\\'])
                && private_dir(dir))
            .then_some(text)
        };
        let Some(text) = usable(dir).or_else(|| usable(&fallback_dir())) else {
            static SAID: std::sync::Once = std::sync::Once::new();
            SAID.call_once(|| {
                crate::log!(
                    "vibed: each SSH command to a host gets a connection of its own: there's no \
                     private directory with a short enough path for a shared one's socket"
                );
            });
            return Vec::new();
        };
        [
            "ControlMaster=auto".to_owned(),
            format!("ControlPath=\"{text}\""),
            format!("ControlPersist={}", SHARED_FOR.as_secs()),
        ]
        .into_iter()
        .flat_map(|o| ["-o".to_owned(), o])
        .collect()
    }

    /// `ssh` running `command` on `address`, over the shared connection
    /// when `share`.
    fn command(&self, address: &str, command: &str, share: bool) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(&self.program);
        c.args(SSH_OPTIONS)
            .args(if share {
                self.sharing(address)
            } else {
                Vec::new()
            })
            .arg("-T")
            .arg("--")
            .arg(address)
            .arg(command)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        c
    }

    /// Runs `command` on `address` with `input` on its stdin, and returns
    /// what it printed. It fails with what `ssh` or the command said.
    async fn run(
        &self,
        address: &str,
        what: &str,
        command: &str,
        input: Vec<u8>,
    ) -> Result<String, String> {
        let mut child = self
            .command(address, command, true)
            .spawn()
            .map_err(|e| format!("can't run {}: {e}", self.program.to_string_lossy()))?;
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let feed = async move {
            // A command that stops reading early is its own error to report.
            let _ = stdin.write_all(&input).await;
            drop(stdin);
        };
        let wait = async {
            let (_, out) = tokio::join!(feed, child.wait_with_output());
            out
        };
        let out = tokio::time::timeout(STEP_TIMEOUT, wait)
            .await
            .map_err(|_| {
                format!(
                    "{what} on {address} took over {} minutes",
                    STEP_TIMEOUT.as_secs() / 60
                )
            })?
            .map_err(|e| format!("{what} on {address}: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            return Ok(stdout);
        }
        let said = report(&stdout)
            .remove("error")
            .or_else(|| last_line(&String::from_utf8_lossy(&out.stderr)))
            .unwrap_or_else(|| format!("it ended with {}", out.status));
        Err(if out.status.code() == Some(255) {
            match self.explain_connect(address, &said).await {
                Some(why) => why,
                None => unreachable_message(address, &said),
            }
        } else {
            format!("{what} on {address} failed: {said}")
        })
    }

    /// When `ssh` couldn't connect to `address` (`said` is its "connect to
    /// host" line), why: from connecting to each of the server's addresses
    /// (see [`crate::reach`]). `None` when it wasn't that, or that adds
    /// nothing.
    async fn explain_connect(&self, address: &str, said: &str) -> Option<String> {
        if !said.contains("connect to host") {
            return None;
        }
        // Where ssh connects: its config may name another host or port.
        let config = tokio::time::timeout(
            Duration::from_secs(5),
            tokio::process::Command::new(&self.program)
                .args(["-G", "--", address])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .ok()?
        .ok()
        .filter(|o| o.status.success())?;
        let target = crate::reach::target(&String::from_utf8_lossy(&config.stdout))?;
        let tried = crate::reach::try_addresses(&target).await;
        crate::reach::explain(address, &target, &tried, cfg!(target_os = "macos"))
    }

    /// `vibe rpc` on `address`, over the shared connection when `share`.
    fn dial_over<'a>(
        &'a self,
        address: &'a str,
        share: bool,
    ) -> BoxFuture<'a, Result<Channel, String>> {
        Box::pin(async move {
            let mut child = self
                .command(address, &format!("{HOST_VIBE} rpc"), share)
                .spawn()
                .map_err(|e| format!("can't run {}: {e}", self.program.to_string_lossy()))?;
            let stdout = child.stdout.take().expect("stdout is piped");
            let stdin = child.stdin.take().expect("stdin is piped");
            let why = keep_last_line(child.stderr.take().expect("stderr is piped"));
            Ok(Channel::new(BufReader::new(stdout), stdin, child, why))
        })
    }

    async fn install_on(&self, address: &str) -> Result<Installed, String> {
        let probe = report(
            &self
                .run(address, "Looking around", "sh -s", PROBE.into())
                .await?,
        );
        let os = probe.get("os").map_or("", String::as_str);
        if os != "Linux" {
            return Err(format!(
                "{address} runs {}, not Linux: a Linux host must run Linux",
                if os.is_empty() {
                    "something unknown"
                } else {
                    os
                }
            ));
        }
        let arch = match probe.get("arch").map(String::as_str) {
            Some("x86_64" | "amd64") => "x86_64",
            Some("aarch64" | "arm64") => "aarch64",
            other => {
                return Err(format!(
                    "{address} has a {} CPU; vibed is built for x86_64 and aarch64",
                    other.unwrap_or("unknown")
                ));
            }
        };
        if probe.get("systemd").map(String::as_str) != Some("yes") {
            return Err(format!(
                "{address} has no systemd user manager to run vibed under (`systemctl --user` \
                 doesn't answer there). VibeOS needs a Linux host with systemd, such as Debian \
                 or Ubuntu"
            ));
        }
        let home = probe
            .get("home")
            .filter(|h| h.starts_with('/'))
            .ok_or_else(|| format!("{address} didn't say where its home directory is"))?
            .clone();
        // Never down: a newer vibed's registry is one this build's can't read, so it
        // couldn't start, and the host would be cut off.
        if let Some(there) = probe.get("ondisk").filter(|v| !v.is_empty()) {
            let mine = env!("CARGO_PKG_VERSION");
            if crate::hosts::older(mine, there) {
                return Err(format!(
                    "{address} has vibed {there}, newer than this VibeOS's {mine}, so it's left \
                     as it is: update VibeOS on this Mac"
                ));
            }
        }
        let bundle = self.bundle_for(arch)?;
        // Each binary goes up only if the server doesn't have it already.
        let mut uploads = Vec::new();
        for name in BINARIES {
            let local = bundle.join(name);
            let read = local.clone();
            let bytes = tokio::task::spawn_blocking(move || std::fs::read(read))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("can't read {}: {e}", local.display()))?;
            let sum = sha256_hex(&bytes);
            if probe.get(&format!("sum.{name}")) != Some(&sum) {
                let into = format!("{HOST_LIB}/.{name}.new");
                // Nothing's left to fill a disk that's nearly full.
                let command = format!(
                    "mkdir -p {HOST_LIB} && {{ cat > {into} || {{ rm -f {into}; exit 1; }}; }}"
                );
                self.run(address, &format!("Uploading {name}"), &command, bytes)
                    .await?;
                uploads.push((name, sum));
            }
        }
        let path = service_path(probe.get("path").map_or("", String::as_str), &home);
        let script = configure_script(&uploads, &unit_file(&path), &path);
        let done = report(
            &self
                .run(address, "Setting up vibed", "sh -s", script.into_bytes())
                .await?,
        );
        let mut notes = Vec::new();
        let mut needs_admin = Vec::new();
        match done.get("linger").map(String::as_str) {
            Some("off") => {
                needs_admin.push(AdminNeed::Linger);
                notes.push(format!(
                "Lingering couldn't be turned on, so vibed stops when you log out of {address}. \
                 Settings → Hosts has a button that does it, asking for the password there once."
            ));
            }
            Some("on") => notes.push(format!(
                "Lingering is on for you on {address}, so vibed runs when you're logged out \
                 and starts again after a reboot."
            )),
            _ => {}
        }
        notes.extend(done.get_all("note"));
        Ok(Installed {
            os: "linux".into(),
            arch: arch.into(),
            label: probe
                .get("label")
                .cloned()
                .unwrap_or_else(|| address.to_owned()),
            version: done
                .get("version")
                .cloned()
                .ok_or_else(|| format!("vibed on {address} didn't say its version"))?,
            missing: done
                .get("missing")
                .map(|m| {
                    m.split(',')
                        .filter(|t| !t.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            needs_admin,
            changed: done.get("changed").map(String::as_str) == Some("yes"),
            notes,
        })
    }

    fn bundle_for(&self, arch: &str) -> Result<PathBuf, String> {
        let Some(bundle) = &self.bundle else {
            return Err(
                "this VibeOS has no Linux build of vibed to install (set VIBE_HOST_BUNDLE to a \
                 directory with one; scripts/host-bundle.sh makes it)"
                    .into(),
            );
        };
        let dir = bundle.join(arch);
        match BINARIES.iter().find(|b| !dir.join(b).is_file()) {
            None => Ok(dir),
            Some(b) => Err(format!(
                "this VibeOS has no {b} for Linux {arch} to install ({} isn't there)",
                dir.join(b).display()
            )),
        }
    }
}

impl Default for Ssh {
    fn default() -> Self {
        Ssh::new()
    }
}

impl Remote for Ssh {
    fn dial<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        self.dial_over(address, false)
    }

    fn dial_link<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Channel, String>> {
        self.dial_over(address, true)
    }

    fn install<'a>(&'a self, address: &'a str) -> BoxFuture<'a, Result<Installed, String>> {
        Box::pin(self.install_on(address))
    }
    fn drop_shared(&self, address: &str) {
        let sharing = self.sharing(address);
        if sharing.is_empty() {
            return;
        }
        // `-O exit` asks the connection's own ssh over its socket, which
        // answers even while the connection under it hangs.
        let exit = tokio::process::Command::new(&self.program)
            .args(sharing)
            .args(["-O", "exit", "--", address])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn();
        if let Ok(mut exit) = exit {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(10), exit.wait()).await;
            });
        }
    }
    fn tunnel<'a>(
        &'a self,
        address: &'a str,
        remote: &'a str,
        local: &'a std::path::Path,
    ) -> BoxFuture<'a, Result<TunnelChild, String>> {
        Box::pin(async move {
            let mut child = tokio::process::Command::new(&self.program)
                .args(tunnel_args(address, remote, local))
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("can't run {}: {e}", self.program.to_string_lossy()))?;
            let stdin = child.stdin.take().expect("stdin is piped");
            let why = keep_last_line(child.stderr.take().expect("stderr is piped"));
            let address = address.to_owned();
            Ok(TunnelChild {
                ended: Box::pin(async move {
                    // Held open while the tunnel is up: once vibed is gone,
                    // however it went, `cat` there reads its end, and the
                    // tunnel closes with it.
                    let _stdin = stdin;
                    let status = child.wait().await;
                    // stderr's last line may come just after the exit.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    let said = why.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    match status {
                        Ok(s) if s.code() == Some(255) && !said.is_empty() => {
                            tunnel_failure(&address, &said)
                        }
                        Ok(s) if said.is_empty() => format!("ssh ended ({s})"),
                        Ok(_) => format!("ssh ended: {said}"),
                        Err(e) => format!("ssh ended: {e}"),
                    }
                }),
            })
        })
    }
}

/// `ssh`'s arguments for the tunnel to `address`: its own connection (not
/// one shared through a control master, which would outlive it), that
/// fails when the forward can't be made, and runs `cat` there to end when
/// its input does.
pub fn tunnel_args(address: &str, remote: &str, local: &std::path::Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = SSH_OPTIONS.iter().map(OsString::from).collect();
    for o in [
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
    ] {
        args.push("-o".into());
        args.push(o.into());
    }
    args.push("-T".into());
    args.push("-R".into());
    let mut forward = OsString::from(remote);
    forward.push(":");
    forward.push(local.as_os_str());
    args.push(forward);
    args.push("--".into());
    args.push(address.into());
    args.push("cat >/dev/null".into());
    args
}

/// Why a tunnel couldn't be made, with how to fix the usual reason.
fn tunnel_failure(address: &str, said: &str) -> String {
    if said.contains("remote port forwarding failed") || said.contains("forwarding failed") {
        format!(
            "{address}'s sshd refused to forward a Unix socket: {said}. Allow it in its \
             /etc/ssh/sshd_config with AllowStreamLocalForwarding yes (the default), then restart sshd"
        )
    } else {
        unreachable_message(address, said)
    }
}

/// What a step printed: `key=value` lines. A key may come more than once.
#[derive(Debug, Default)]
struct Report(Vec<(String, String)>);

impl Report {
    fn get(&self, key: &str) -> Option<&String> {
        self.0.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn get_all(&self, key: &str) -> Vec<String> {
        self.0
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .collect()
    }

    fn remove(&mut self, key: &str) -> Option<String> {
        let v = self.get(key).cloned();
        self.0.retain(|(k, _)| k != key);
        v
    }
}

fn report(stdout: &str) -> Report {
    Report(
        stdout
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect(),
    )
}

fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.contains(NOT_SHARING))
        .map(|l| l.chars().take(WHY_CHARS).collect())
}

/// Why `ssh` couldn't get to `address`, with what helps for the usual
/// reasons.
pub fn unreachable_message(address: &str, said: &str) -> String {
    let hint = if said.contains("Permission denied") {
        format!(
            " VibeOS signs in with your SSH keys and can't answer a password or passphrase \
             prompt: check that `ssh {address}` works in Terminal without one (add your key to \
             the agent with `ssh-add --apple-use-keychain`)."
        )
    } else if said.contains("Host key verification failed")
        || said.contains("HOST IDENTIFICATION HAS CHANGED")
    {
        format!(
            " {address}'s host key isn't the one known for it. If the server was rebuilt, \
             remove the old key with `ssh-keygen -R` and connect once in Terminal."
        )
    } else if said.contains("kex_exchange_identification") || said.contains("banner exchange") {
        " The server closed the connection before SSH started. sshd turns clients away \
         like this when they connect often (its MaxStartups or PerSourcePenalties): wait a \
         minute, then try again."
            .to_owned()
    } else {
        String::new()
    };
    format!(
        "can't reach {address} over SSH: {}.{hint}",
        said.trim_end_matches('.')
    )
}

/// Where a shared connection's socket goes when the run directory's path is
/// too long: one of this user's own under `/tmp`.
fn fallback_dir() -> PathBuf {
    // SAFETY: getuid can't fail.
    PathBuf::from(format!("/tmp/vibeos-{}", unsafe { libc::getuid() }))
}

/// Makes `dir` if it isn't there, only its owner's, and says whether it's a
/// directory (not a link to one) no one else may write to.
fn private_dir(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let _ = std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir);
    std::fs::symlink_metadata(dir).is_ok_and(|m| {
        // SAFETY: getuid can't fail.
        m.is_dir() && m.mode() & 0o022 == 0 && m.uid() == unsafe { libc::getuid() }
    })
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The Linux builds this `vibed` installs on hosts: `$VIBE_HOST_BUNDLE`, or
/// `linux/` beside `vibed`, or in the app bundle's `Resources/`.
pub fn default_bundle() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("VIBE_HOST_BUNDLE").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [dir.join("linux"), dir.join("../Resources/linux")]
        .into_iter()
        .find(|d| d.is_dir())
}

// The scripts. They run with `sh -s` on the server, in its home directory,
// and print `key=value` lines; `error=` says why one failed.

/// What the server is, what it has, and what it has of VibeOS already.
const PROBE: &str = r#"
echo "os=$(uname -s)"
echo "arch=$(uname -m)"
echo "label=$(hostname 2>/dev/null || uname -n)"
echo "home=$HOME"
lib="$HOME/.local/lib/vibeos"
if [ -x "$lib/vibed" ]; then
  echo "ondisk=$("$lib/vibed" --version 2>/dev/null | cut -d ' ' -f 2)"
fi
for f in vibed vibe vibe-shim; do
  # What an upload cut short left behind.
  rm -f "$lib/.$f.new"
  if [ -f "$lib/$f" ] && command -v sha256sum >/dev/null 2>&1; then
    echo "sum.$f=$(sha256sum "$lib/$f" | cut -d ' ' -f 1)"
  fi
done
if command -v systemctl >/dev/null 2>&1 && systemctl --user show-environment >/dev/null 2>&1; then
  echo "systemd=yes"
else
  echo "systemd=no"
fi
login_path=$("${SHELL:-/bin/sh}" -lc 'printenv PATH' </dev/null 2>/dev/null | tail -n 1)
echo "path=${login_path:-$PATH}"
"#;

/// `bytes` in standard base64: only letters, digits, `+`, `/` and `=`.
fn base64(bytes: &[u8]) -> String {
    const LETTERS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |n, b| (n << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(LETTERS[(n >> (18 - 6 * i)) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The `PATH` `vibed` runs with on the server: the owner's login shell's,
/// where `claude` and `gh` are, with `~/.local/bin`, where Claude Code's
/// installer puts it.
fn service_path(login: &str, home: &str) -> String {
    let mut dirs: Vec<&str> = login
        .split(':')
        .filter(|d| d.starts_with('/') && !d.contains(['\n', '"', '\\']))
        .collect();
    if dirs.is_empty() {
        dirs = vec!["/usr/local/bin", "/usr/bin", "/bin"];
    }
    let local_bin = format!("{home}/.local/bin");
    let mut path = dirs.join(":");
    if !dirs.contains(&local_bin.as_str()) {
        path = format!("{local_bin}:{path}");
    }
    path
}

/// `vibed`'s systemd user service. `KillMode=process`: agents' shims run in
/// this unit's cgroup, and stopping or restarting `vibed` must leave them
/// running (PRD: Restarts).
pub fn unit_file(path: &str) -> String {
    format!(
        "[Unit]\n\
         Description=VibeOS daemon\n\
         Documentation=https://github.com/tsconfigdotjson/vibe-os-native\n\
         \n\
         [Service]\n\
         ExecStart=%h/{HOST_LIB}/vibed\n\
         Environment=\"PATH={}\"\n\
         Restart=on-failure\n\
         RestartSec=2\n\
         # Agents' sessions run in this unit's cgroup: restarting vibed leaves them.\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        path.replace('%', "%%")
    )
}

/// What an install says of a build that wouldn't start, having put back the one before.
/// It's how this Mac knows its own build is what's wrong: reinstalling it would do harm.
pub const WENT_BACK: &str = "it went back to the one before";

/// Puts the uploaded binaries in place, checked against their sums, writes
/// the service if it changed, turns lingering on, and (re)starts `vibed`
/// until it answers. Then says its version and which tools are missing on
/// `path`.
fn configure_script(uploads: &[(&str, String)], unit: &str, path: &str) -> String {
    let mut s = String::from(
        "set -u\n\
         lib=\"$HOME/.local/lib/vibeos\"\n\
         bin=\"$HOME/.local/bin\"\n\
         units=\"$HOME/.config/systemd/user\"\n\
         mkdir -p \"$lib\" \"$bin\" \"$units\" || { echo \"error=can't make $lib\"; exit 1; }\n\
         changed=no\n",
    );
    for (name, sum) in uploads {
        s.push_str(&format!(
            "got=$(sha256sum \"$lib/.{name}.new\" 2>/dev/null | cut -d ' ' -f 1)\n\
             if [ \"$got\" != {sum} ] && command -v sha256sum >/dev/null 2>&1; then\n  \
               echo \"error={name} didn't arrive whole\"; exit 1\n\
             fi\n\
             rm -f \"$lib/.{name}.prev\"\n\
             if [ -f \"$lib/{name}\" ]; then cp -p \"$lib/{name}\" \"$lib/.{name}.prev\"; fi\n\
             chmod 755 \"$lib/.{name}.new\" && mv -f \"$lib/.{name}.new\" \"$lib/{name}\" || \
             {{ echo \"error=can't put {name} in place\"; exit 1; }}\n\
             changed=yes\n"
        ));
    }
    s.push_str(
        "for f in vibe vibed; do\n  \
           if [ -L \"$bin/$f\" ] || [ ! -e \"$bin/$f\" ]; then\n    \
             ln -sfn \"../lib/vibeos/$f\" \"$bin/$f\"\n  \
           else\n    \
             echo \"note=$bin/$f isn't VibeOS's, so it was left as it is: VibeOS's is $lib/$f.\"\n  \
           fi\n\
         done\n\
         unit=\"$units/vibed.service\"\n\
         new_unit=$(cat <<'VIBEOS_UNIT'\n",
    );
    s.push_str(unit);
    s.push_str(
        "VIBEOS_UNIT\n\
         )\n\
         if [ ! -f \"$unit\" ] || [ \"$(cat \"$unit\")\" != \"$new_unit\" ]; then\n  \
           printf '%s\\n' \"$new_unit\" > \"$unit.new\" && mv -f \"$unit.new\" \"$unit\" || \
           { echo \"error=can't write $unit\"; exit 1; }\n  \
           changed=yes\n\
         fi\n\
         me=$(id -un)\n\
         if [ \"$(loginctl show-user \"$me\" -p Linger 2>/dev/null)\" != Linger=yes ]; then\n  \
           if loginctl enable-linger \"$me\" >/dev/null 2>&1 || sudo -n loginctl enable-linger \"$me\" >/dev/null 2>&1; then \
             echo linger=on; else echo linger=off; fi\n\
         fi\n\
         systemctl --user daemon-reload || { echo \"error=systemctl --user daemon-reload failed\"; exit 1; }\n\
         systemctl --user enable vibed.service >/dev/null 2>&1 || \
           { echo \"error=systemctl --user enable vibed.service failed\"; exit 1; }\n\
         # An upgrade cut short after its files went in, whose vibed is still the one\n\
         # before: nothing's new to go up, and it still has to come to the new one.\n\
         main=$(systemctl --user show -p MainPID --value vibed.service 2>/dev/null)\n\
         case \"$(readlink \"/proc/$main/exe\" 2>/dev/null)\" in\n  \
           *\" (deleted)\") changed=yes ;;\n\
         esac\n\
         if [ \"$changed\" = yes ]; then action=restart; else action=start; fi\n\
         systemctl --user \"$action\" vibed.service || \
           { echo \"error=systemctl --user $action vibed.service failed\"; exit 1; }\n\
         i=0\n\
         until \"$lib/vibe\" hello >/dev/null 2>&1; do\n  \
           i=$((i + 1))\n  \
           if [ \"$i\" -gt 30 ]; then\n    \
             why=$(journalctl --user -u vibed.service -t vibed -n 1 --no-pager -o cat 2>/dev/null)\n    \
             # A build that won't start isn't left to cut the host off.\n    \
             back=no\n    \
             for f in vibed vibe vibe-shim; do\n      \
               if [ -f \"$lib/.$f.prev\" ]; then mv -f \"$lib/.$f.prev\" \"$lib/$f\"; back=yes; fi\n    \
             done\n    \
             if [ \"$back\" = yes ]; then\n      \
               systemctl --user restart vibed.service\n      \
               echo \"error=vibed didn't start on this build, so it went back to the one before: $why\"\n    \
             else\n      \
               echo \"error=vibed didn't start: $why\"\n    \
             fi\n    \
             exit 1\n  \
           fi\n  \
           sleep 1\n\
         done\n\
         rm -f \"$lib\"/.*.prev\n\
         echo \"version=$(\"$lib/vibed\" --version | cut -d ' ' -f 2)\"\n\
         echo \"changed=$changed\"\n",
    );
    s.push_str(&format!(
        "PATH={}:/usr/local/bin:/opt/homebrew/bin\nmissing=\n\
         for t in {}; do command -v \"$t\" >/dev/null 2>&1 || missing=\"$missing,$t\"; done\n\
         echo \"missing=${{missing#,}}\"\n",
        shell_quote(path),
        TOOLS.join(" ")
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_install_says_it_went_back_in_the_words_that_are_looked_for() {
        let script = configure_script(&[("vibed", "sum".to_owned())], "unit", "/bin");
        assert!(script.contains(WENT_BACK), "{script}");
    }

    /// A channel to a far end the test plays, and that far end.
    fn a_channel() -> (Channel, tokio::io::DuplexStream) {
        let (ours, theirs) = tokio::io::duplex(1 << 16);
        let (read, write) = tokio::io::split(ours);
        let channel = Channel::new(
            tokio::io::BufReader::new(read),
            write,
            (),
            Arc::new(Mutex::new(String::new())),
        );
        (channel, theirs)
    }

    #[tokio::test]
    async fn a_call_nobody_takes_fails_at_the_accept_deadline() {
        let (mut channel, _silent) = a_channel();
        let begun = std::time::Instant::now();
        let e = channel
            .call_accepted("agents.start", json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);
        assert!(e.to_string().contains("didn't take agents.start"), "{e}");
        assert!(begun.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_call_taken_may_take_longer_than_the_deadline() {
        use tokio::io::AsyncWriteExt;
        let (mut channel, mut far) = a_channel();
        let far_end = tokio::spawn(async move {
            let accepted = json!({"jsonrpc": "2.0", "method": CALL_ACCEPTED, "params": {"id": 1}});
            far.write_all(format!("{accepted}\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(400)).await;
            let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {"done": true}});
            far.write_all(format!("{answer}\n").as_bytes())
                .await
                .unwrap();
            far
        });
        let answer = channel
            .call_accepted("agents.start", json!({}), Duration::from_millis(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer["done"], true);
        far_end.await.unwrap();
    }

    #[tokio::test]
    async fn only_a_calls_own_accepted_counts_and_an_answer_needs_none() {
        use tokio::io::AsyncWriteExt;
        let (mut channel, mut far) = a_channel();
        let other = json!({"jsonrpc": "2.0", "method": CALL_ACCEPTED, "params": {"id": 99}});
        far.write_all(format!("{other}\n").as_bytes())
            .await
            .unwrap();
        let e = channel
            .call_accepted("agents.start", json!({}), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::TimedOut);

        let (mut channel, mut far) = a_channel();
        let answer = json!({"jsonrpc": "2.0", "id": 1, "result": {"done": true}});
        far.write_all(format!("{answer}\n").as_bytes())
            .await
            .unwrap();
        let answer = channel
            .call_accepted("agents.list", json!({}), Duration::from_millis(100))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(answer["done"], true);
    }

    #[test]
    fn the_tunnel_is_its_own_ssh_and_fails_when_its_forward_does() {
        let args: Vec<String> = tunnel_args(
            "me@vps",
            "/home/me/.local/share/vibeos/run/mac.sock",
            std::path::Path::new("/Users/me/Library/Application Support/VibeOS/run/tunnel.sock"),
        )
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
        let joined = args.join(" ");
        for want in [
            "BatchMode=yes",
            "ForwardAgent=no",
            "ExitOnForwardFailure=yes",
            "ControlMaster=no",
            "ControlPath=none",
        ] {
            assert!(joined.contains(want), "{joined}");
        }
        let r = args.iter().position(|a| a == "-R").unwrap();
        assert_eq!(
            args[r + 1],
            "/home/me/.local/share/vibeos/run/mac.sock:/Users/me/Library/Application Support/VibeOS/run/tunnel.sock"
        );
        // The destination comes after `--`, so it's never read as an option.
        let dd = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(&args[dd + 1..], ["me@vps", "cat >/dev/null"]);
        assert!(
            tunnel_failure(
                "me@vps",
                "Error: remote port forwarding failed for listen path x"
            )
            .contains("AllowStreamLocalForwarding")
        );
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("me@vps.example.com"), "me@vps.example.com");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    /// A real shell here, and one standing in for the server's, with stand-ins for
    /// `ssh` and `sudo`. Says what `sudo` was asked, and what came of it.
    fn run_admin_line(line: &str, sudo_body: &str) -> (std::process::Output, String) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let write = |name: &str, body: &str| {
            let p = tmp.path().join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        write("ssh", "shift 2\nexec /bin/sh -c \"$*\"");
        write(
            "sudo",
            &format!(
                "printf '%s\\n' \"$*\" >> {}/asked\n{sudo_body}",
                tmp.path().display()
            ),
        );
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(line)
            .env("PATH", format!("{}:/usr/bin:/bin", tmp.path().display()))
            .output()
            .unwrap();
        let asked = std::fs::read_to_string(tmp.path().join("asked")).unwrap_or_default();
        (out, asked)
    }

    #[test]
    fn base64_is_what_base64_reads() {
        for text in ["", "a", "ab", "abc", "abcd", "it's \"quoted\"\nand long"] {
            let mut child = std::process::Command::new("base64")
                .arg("-d")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(base64(text.as_bytes()).as_bytes())
                .unwrap();
            let out = child.wait_with_output().unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), text);
        }
    }

    #[test]
    fn an_admin_line_is_ssh_and_sudo_and_nothing_a_host_said() {
        let ssh = Ssh {
            program: "ssh".into(),
            bundle: None,
            shared: None,
        };
        let line = ssh.admin_line("me@vps", &[AdminNeed::Linger]);
        let inner = line
            .strip_prefix("ssh -t me@vps 'echo ")
            .and_then(|l| l.strip_suffix(" | base64 -d | sh'"))
            .unwrap_or_else(|| panic!("{line}"));
        // What any login shell reads the same: no quotes or newlines of its own.
        assert!(
            inner
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+/=".contains(c)),
            "{inner}"
        );
        // A destination with anything odd in it can't reach the shell here.
        let odd = ssh.admin_line("me@vps; rm -rf ~", &[AdminNeed::Linger]);
        assert!(odd.starts_with("ssh -t 'me@vps; rm -rf ~' 'echo "), "{odd}");

        let (out, asked) = run_admin_line(&line, "exit 0");
        assert!(out.status.success(), "{out:?}");
        assert!(String::from_utf8_lossy(&out.stdout).contains("VibeOS: done"));
        let me = String::from_utf8(
            std::process::Command::new("id")
                .arg("-un")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(asked, format!("loginctl enable-linger {me}"));

        // Refused: not called done, and it fails.
        let (out, _) = run_admin_line(&line, "exit 1");
        assert!(!out.status.success(), "{out:?}");
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(!said.contains("done"), "{said}");
        assert!(said.contains("didn't work"), "{said}");
    }

    #[test]
    fn attach_lines_name_the_host_and_ref() {
        assert_eq!(
            Ssh {
                program: "ssh".into(),
                bundle: None,
                shared: None,
            }
            .attach_line("me@vps", "quiet-amber-otter"),
            "ssh -t me@vps .local/lib/vibeos/vibe attach quiet-amber-otter"
        );
        // A host that answers with a ref no host would make can't have the
        // Mac's shell run what it likes.
        let ssh = Ssh {
            program: "ssh".into(),
            bundle: None,
            shared: None,
        };
        assert_eq!(
            ssh.attach_line("me@vps", "x; rm -rf ~"),
            "ssh -t me@vps .local/lib/vibeos/vibe attach 'x; rm -rf ~'"
        );
    }

    #[test]
    fn reports() {
        let r = report("os=Linux\nnoise\nnote=a\nnote=b=c\narch = x86_64\n");
        assert_eq!(r.get("os").map(String::as_str), Some("Linux"));
        assert_eq!(r.get("arch").map(String::as_str), Some("x86_64"));
        assert_eq!(r.get_all("note"), ["a", "b=c"]);
        assert_eq!(r.get("nope"), None);
    }

    #[test]
    fn the_service_path_has_the_login_shells_and_local_bin() {
        assert_eq!(
            service_path("/usr/bin:/bin", "/home/me"),
            "/home/me/.local/bin:/usr/bin:/bin"
        );
        assert_eq!(
            service_path("/home/me/.local/bin:/usr/bin", "/home/me"),
            "/home/me/.local/bin:/usr/bin"
        );
        // Relative and odd entries are dropped; nothing at all gets a default.
        assert_eq!(
            service_path("bin:/a\"b:/usr/bin", "/h"),
            "/h/.local/bin:/usr/bin"
        );
        assert_eq!(
            service_path("", "/h"),
            "/h/.local/bin:/usr/local/bin:/usr/bin:/bin"
        );
    }

    #[test]
    fn the_unit_keeps_sessions_when_vibed_restarts() {
        let unit = unit_file("/h/.local/bin:/odd%dir");
        assert!(unit.contains("KillMode=process\n"), "{unit}");
        assert!(
            unit.contains("ExecStart=%h/.local/lib/vibeos/vibed\n"),
            "{unit}"
        );
        assert!(
            unit.contains("Environment=\"PATH=/h/.local/bin:/odd%%dir\"\n"),
            "{unit}"
        );
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
    }

    /// Issue #39: an install's five commands, and the link after them,
    /// share one connection, so a server that limits how often a client may
    /// connect doesn't turn the later ones away.
    #[test]
    fn a_hosts_commands_share_one_connection() {
        use std::os::unix::fs::PermissionsExt;
        let run = tempfile::Builder::new()
            .prefix("vibe")
            .tempdir_in("/tmp")
            .unwrap();
        let dir = run.path().join("run");
        let ssh = Ssh {
            program: "ssh".into(),
            bundle: None,
            shared: Some(dir.clone()),
        };
        let name = format!("ssh-{}", &sha256_hex(b"me@vps")[..16]);
        let options = |ssh: &Ssh| ssh.sharing("me@vps");
        let path_in = |options: Vec<String>| {
            options
                .iter()
                .find_map(|o| o.strip_prefix("ControlPath="))
                .map(|p| p.trim_matches('"').to_owned())
        };
        assert_eq!(
            options(&ssh),
            [
                "-o",
                "ControlMaster=auto",
                "-o",
                &format!("ControlPath=\"{}\"", dir.join(&name).display()),
                "-o",
                "ControlPersist=60",
            ]
        );
        // The directory is made, and only its owner's.
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        // Each destination has its own.
        assert_ne!(ssh.sharing("me@other"), options(&ssh));
        // Install steps share, before `--`; a call's channel doesn't.
        let args = |share| {
            ssh.command("me@vps", "true", share)
                .as_std()
                .get_args()
                .map(|a| a.to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let shared = args(true);
        let dd = shared.iter().position(|a| a == "--").unwrap();
        assert!(
            shared[..dd].contains(&"ControlMaster=auto".to_owned()),
            "{shared:?}"
        );
        assert!(!args(false).iter().any(|a| a.starts_with("Control")));

        // None without a directory.
        let without = Ssh {
            shared: None,
            ..ssh.clone()
        };
        assert!(without.sharing("me@vps").is_empty());
        // A path too long for a socket, one ssh would read otherwise, or a
        // directory others may write to: this user's own under /tmp.
        let fallback = fallback_dir().join(&name).display().to_string();
        let open = run.path().join("open");
        std::fs::create_dir(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        for dir in [
            run.path().join("x".repeat(90)),
            run.path().join("100%"),
            run.path().join("back\\slash"),
            open,
        ] {
            let other = Ssh {
                shared: Some(dir.clone()),
                ..ssh.clone()
            };
            assert_eq!(
                path_in(other.sharing("me@vps")).as_deref(),
                Some(fallback.as_str()),
                "{}",
                dir.display()
            );
        }
        let mode = std::fs::metadata(fallback_dir())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    /// With real OpenSSH and a server that takes `VIBE_TEST_SSH_DEST`
    /// without a prompt (`VIBE_TEST_SSH` may name an `ssh` wrapper adding
    /// options): an install's worth of commands go over one connection,
    /// which is still up after them, until it's dropped.
    #[tokio::test]
    #[ignore = "needs an SSH server: VIBE_TEST_SSH_DEST"]
    async fn real_ssh_shares_one_connection() {
        let dest = std::env::var("VIBE_TEST_SSH_DEST").expect("VIBE_TEST_SSH_DEST");
        let program: OsString = std::env::var_os("VIBE_TEST_SSH").unwrap_or_else(|| "ssh".into());
        // Under /tmp: a socket's path must be short.
        let run = tempfile::Builder::new()
            .prefix("vibe")
            .tempdir_in("/tmp")
            .unwrap();
        let ssh = Ssh {
            program: program.clone(),
            bundle: None,
            shared: Some(run.path().join("run")),
        };
        for step in 0..5 {
            let said = ssh
                .run(&dest, "A step", "cat; echo done", vec![b'x'; 1 << 20])
                .await
                .unwrap();
            assert!(said.ends_with("done\n"), "step {step}: {said:?}");
        }
        let check = std::process::Command::new(&program)
            .args(ssh.sharing(&dest))
            .args(["-O", "check", "--", &dest])
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "{}",
            String::from_utf8_lossy(&check.stderr)
        );
        // Once it stops answering, the link drops it, and it's gone.
        ssh.drop_shared(&dest);
        let mut gone = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let check = std::process::Command::new(&program)
                .args(ssh.sharing(&dest))
                .args(["-O", "check", "--", &dest])
                .output()
                .unwrap();
            if !check.status.success() {
                gone = true;
                break;
            }
        }
        assert!(gone, "the shared connection is still up");
    }

    #[test]
    fn not_sharing_is_no_reason() {
        let said = "ssh: connect to host vps port 22: Connection refused\n\
                    ControlSocket /x/ssh-1 already exists, disabling multiplexing\n";
        assert_eq!(
            last_line(said).as_deref(),
            Some("ssh: connect to host vps port 22: Connection refused")
        );
    }

    #[test]
    fn unreachable_hosts_say_how_to_fix_the_usual_reasons() {
        let m = unreachable_message("me@vps", "me@vps: Permission denied (publickey)");
        assert!(
            m.starts_with("can't reach me@vps over SSH: me@vps: Permission denied"),
            "{m}"
        );
        assert!(m.contains("without one"), "{m}");
        let m = unreachable_message(
            "vps",
            "ssh: connect to host vps port 22: Network is unreachable",
        );
        assert_eq!(
            m,
            "can't reach vps over SSH: ssh: connect to host vps port 22: Network is unreachable."
        );
        let m = unreachable_message(
            "vps",
            "kex_exchange_identification: Connection closed by remote host",
        );
        assert!(m.contains("wait a minute"), "{m}");
    }
}
