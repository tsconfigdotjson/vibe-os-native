//! Driving `vibe-vm`, the Swift helper that runs one `linux` agent's VM on
//! the Mac (PRD: Architecture, Sleep and wake). The contract is in
//! `protocol/vibe-vm/`, and both sides are tested against it.
//!
//! A VM's life:
//!
//! 1. [`create`] once, when the agent is made: `vibe-vm create` gives the
//!    VM its machine identifier and MAC address, writes them into the
//!    agent's `vm.json`, and checks the VM can be saved and restored. The
//!    configuration is then fixed for the agent's life, because a snapshot
//!    only restores into the exact configuration it was saved from.
//! 2. [`boot`] or [`restore`]: starts a detached `vibe-vm`, which prints one
//!    line on the pipe it was started with (the [`Ready`] line, with
//!    timings), then serves its control socket, `run/<ref>.vm`.
//! 3. On that socket: [`status`], [`connect`] (a stream to a vsock port in
//!    the guest), [`sleep`] (balloon, pause, save the snapshot, stop and
//!    exit) and [`stop`].
//!
//! A restarted `vibed` finds a running `vibe-vm` again by its socket
//! ([`probe`]), as it finds shims.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use vibe_core::model::validate_ref;
use vibe_core::socket::lock_path;

/// The control protocol's version, in [`Ready`] and [`Status`].
pub const PROTOCOL: u32 = 1;

/// The version of `vm.json` this code reads and writes.
pub const CONFIG_VERSION: u32 = 1;

/// The longest disk id: Virtualization.framework's limit for a virtio block
/// device's serial, which is how the guest tells its disks apart.
pub const MAX_DISK_ID: usize = 20;

/// The longest virtio-fs tag.
pub const MAX_SHARE_TAG: usize = 36;

/// The longest reply line anyone sends.
const MAX_LINE: usize = 64 * 1024;

const MIB: u64 = 1 << 20;

/// How long `vibe-vm` may take to boot or restore. A restore of a VM with a
/// lot of memory in use takes seconds; a boot under a second.
const START_TIMEOUT: Duration = Duration::from_secs(120);

/// How long `vibe-vm` may take to answer anything but a sleep.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a save may take on top of the balloon's time: a 4 GB VM with
/// every page in use writes 4 GB.
const SAVE_TIMEOUT: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// The configuration

/// A `linux` agent's VM, as `vibed` writes it to `vm.json`. Everything here
/// is fixed for the agent's life: a snapshot restores only into the exact
/// configuration it was saved from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmConfig {
    pub version: u32,
    /// The agent's ref.
    #[serde(rename = "ref")]
    pub agent: String,
    /// vCPUs, from the host limits ([`vcpus`]).
    pub cpus: u32,
    /// RAM in bytes, from the profile: a whole number of MiB.
    pub memory: u64,
    /// An uncompressed arm64 Linux kernel image.
    pub kernel: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initrd: Option<PathBuf>,
    pub cmdline: String,
    /// Virtualization.framework's machine identifier, base64. `vibe-vm
    /// create` makes it; `boot` refuses a configuration without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
    /// The NIC's MAC address. `vibe-vm create` picks a random locally
    /// administered one if it's absent: the default, random on every start,
    /// breaks restoring.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
    /// Block devices, in order: the read-only layers, then the agent's own
    /// writable image.
    pub disks: Vec<Disk>,
    /// virtio-fs shares.
    pub shares: Vec<Share>,
    /// Where the guest's console (`hvc0`) goes. Without one it's dropped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_log: Option<PathBuf>,
    /// Where a sleep saves the VM, and a restore reads it from.
    pub snapshot: PathBuf,
}

/// A block device. The guest finds it by `id`, its virtio serial
/// (`/sys/block/vd*/serial`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Disk {
    pub id: String,
    /// A raw disk image.
    pub path: PathBuf,
    pub read_only: bool,
}

/// A host directory shared with the guest over virtio-fs, under `tag`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub tag: String,
    pub path: PathBuf,
    pub read_only: bool,
}

impl VmConfig {
    /// Checks what can be checked without Virtualization.framework, with
    /// the same rules as `vibe-vm`.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != CONFIG_VERSION {
            return Err(format!(
                "version {} isn't supported; this reads version {CONFIG_VERSION}",
                self.version
            ));
        }
        validate_ref(&self.agent)?;
        if self.cpus == 0 {
            return Err("cpus must be at least 1".into());
        }
        if self.memory == 0 || !self.memory.is_multiple_of(MIB) {
            return Err(format!(
                "memory must be a whole number of MiB, not {} bytes",
                self.memory
            ));
        }
        absolute("kernel", &self.kernel)?;
        if let Some(initrd) = &self.initrd {
            absolute("initrd", initrd)?;
        }
        if let Some(log) = &self.console_log {
            absolute("console_log", log)?;
        }
        absolute("snapshot", &self.snapshot)?;
        if let Some(mac) = &self.mac {
            parse_mac(mac)?;
        }
        if let Some(id) = &self.machine_id {
            let base64 = |c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=');
            if id.is_empty() || !id.chars().all(base64) {
                return Err("machine_id must be base64".into());
            }
        }
        let mut ids = BTreeSet::new();
        for disk in &self.disks {
            label("disk id", &disk.id, MAX_DISK_ID)?;
            if !ids.insert(&disk.id) {
                return Err(format!("disk id `{}` is used twice", disk.id));
            }
            absolute(&format!("disk `{}`", disk.id), &disk.path)?;
        }
        let mut tags = BTreeSet::new();
        for share in &self.shares {
            label("share tag", &share.tag, MAX_SHARE_TAG)?;
            if !tags.insert(&share.tag) {
                return Err(format!("share tag `{}` is used twice", share.tag));
            }
            absolute(&format!("share `{}`", share.tag), &share.path)?;
        }
        Ok(())
    }

    /// Reads and checks `vm.json`.
    pub fn load(path: &Path) -> Result<VmConfig, VmError> {
        let text = fs::read_to_string(path).map_err(io_err(format!("read {}", path.display())))?;
        let config: VmConfig = serde_json::from_str(&text)
            .map_err(|e| VmError::Config(format!("{}: {e}", path.display())))?;
        config
            .validate()
            .map_err(|e| VmError::Config(format!("{}: {e}", path.display())))?;
        Ok(config)
    }

    /// Writes `vm.json`, replacing it in one step.
    pub fn save(&self, path: &Path) -> Result<(), VmError> {
        self.validate().map_err(VmError::Config)?;
        let mut text = serde_json::to_string_pretty(self).expect("a config always encodes");
        text.push('\n');
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let what = format!("write {}", path.display());
        fs::write(&tmp, text).map_err(io_err(what.clone()))?;
        fs::rename(&tmp, path).map_err(io_err(what))
    }
}

fn absolute(what: &str, path: &Path) -> Result<(), String> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(format!(
            "{what} must be an absolute path, not `{}`",
            path.display()
        ))
    }
}

/// A disk id or share tag: what the guest looks devices up by.
fn label(what: &str, s: &str, max: usize) -> Result<(), String> {
    let ok = !s.is_empty()
        && s.len() <= max
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(format!(
            "{what} `{s}` must be 1 to {max} letters, digits, `.`, `_` and `-`"
        ))
    }
}

/// Parses a unicast MAC address written `02:5f:3a:11:9c:e4`.
pub fn parse_mac(s: &str) -> Result<[u8; 6], String> {
    let bad = || format!("`{s}` isn't a MAC address like 02:5f:3a:11:9c:e4");
    let mut mac = [0u8; 6];
    let mut parts = s.split(':');
    for byte in &mut mac {
        let part = parts.next().ok_or_else(bad)?;
        if part.len() != 2 {
            return Err(bad());
        }
        *byte = u8::from_str_radix(part, 16).map_err(|_| bad())?;
    }
    if parts.next().is_some() {
        return Err(bad());
    }
    if mac[0] & 1 != 0 {
        return Err(format!(
            "`{s}` is a multicast address; a NIC needs a unicast one"
        ));
    }
    Ok(mac)
}

/// A `linux` agent's vCPUs: the host limits' `cpu_cores`, but never more
/// than the Mac has, nor fewer than one.
pub fn vcpus(cpu_cores: u32, host_cpus: usize) -> u32 {
    let host = u32::try_from(host_cpus).unwrap_or(u32::MAX).max(1);
    cpu_cores.clamp(1, host)
}

/// Makes an agent's writable disk: an empty, sparse raw image of `size`
/// bytes, which the guest formats on first boot. It never replaces a file.
pub fn create_disk(path: &Path, size: u64) -> Result<(), VmError> {
    let what = format!("create {}", path.display());
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(io_err(what.clone()))?;
    file.set_len(size).map_err(io_err(what))
}

// ---------------------------------------------------------------------------
// The protocol

/// A request on the control socket: one JSON line, then one reply line.
/// After a successful `connect`, the connection is the guest's port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Inflate and release the balloon, pause, save the snapshot, stop and
    /// exit. `vibe-guest` must have frozen the agent first.
    Sleep {
        /// How long the balloon stays inflated: absent for vibe-vm's
        /// default, 0 to skip it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        balloon_ms: Option<u64>,
    },
    /// Stops the VM at once, without saving, and exits.
    Stop,
    /// Makes this connection a stream to `port` in the guest.
    Connect {
        port: u32,
    },
}

/// How a `vibe-vm` started its VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Started {
    Boot,
    Restore,
}

/// Virtualization.framework's state of the VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Starting,
    Running,
    Pausing,
    Paused,
    Resuming,
    Saving,
    Restoring,
    Stopping,
    Stopped,
    Error,
}

/// Why `vibe-vm` refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request isn't one `vibe-vm` knows.
    BadRequest,
    /// `vm.json` is invalid, or lacks the identity `create` gives it.
    BadConfig,
    /// Another `vibe-vm` holds this socket.
    AlreadyRunning,
    /// This Mac or binary can't run the VM, or can't save and restore it.
    Unsupported,
    /// A restore found no snapshot.
    NoSnapshot,
    BootFailed,
    /// The snapshot is there but won't restore; it's kept for `vibed` to
    /// discard (PRD: Failures).
    RestoreFailed,
    /// Another request is changing the VM's state.
    Busy,
    /// The VM is in a state the request can't run in.
    BadState,
    /// The save failed; the VM was resumed, and nothing was left behind.
    SaveFailed,
    ConnectFailed,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 11] = [
        ErrorCode::BadRequest,
        ErrorCode::BadConfig,
        ErrorCode::AlreadyRunning,
        ErrorCode::Unsupported,
        ErrorCode::NoSnapshot,
        ErrorCode::BootFailed,
        ErrorCode::RestoreFailed,
        ErrorCode::Busy,
        ErrorCode::BadState,
        ErrorCode::SaveFailed,
        ErrorCode::ConnectFailed,
    ];
}

/// Durations of the steps of a start or a sleep, in milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timings {
    /// Boot: starting the VM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_ms: Option<u64>,
    /// Wake: reading the snapshot back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_ms: Option<u64>,
    /// Sleep: the balloon inflated, then released.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balloon_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub save_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_ms: Option<u64>,
    /// The whole start (from the process starting) or sleep.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_ms: Option<u64>,
}

/// What `vibe-vm boot` and `restore` print once the VM runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ready {
    pub protocol: u32,
    pub pid: u32,
    pub started: Started,
    pub timings: Timings,
}

/// What `vibe-vm create` prints: the identity it gave the VM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    pub machine_id: String,
    pub mac: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub protocol: u32,
    #[serde(rename = "ref")]
    pub agent: String,
    pub pid: u32,
    pub state: VmState,
    pub started: Started,
}

/// A sleep's reply, sent once the VM is saved and stopped; `vibe-vm` exits
/// right after.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slept {
    pub snapshot: PathBuf,
    /// The snapshot's size on disk.
    pub snapshot_bytes: u64,
    pub timings: Timings,
}

/// The reply to `connect` and `stop`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub error: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

/// One reply line: `{"ok":true, ...}` with `T`'s fields, or `{"ok":false,
/// "code":..., "error":...}`.
pub fn encode_reply<T: Serialize>(reply: &Result<T, Failure>) -> String {
    let (ok, value) = match reply {
        Ok(t) => (true, serde_json::to_value(t)),
        Err(f) => (false, serde_json::to_value(f)),
    };
    let mut map = Map::new();
    map.insert("ok".into(), Value::Bool(ok));
    if let Ok(Value::Object(fields)) = value {
        map.extend(fields);
    }
    Value::Object(map).to_string()
}

/// Reads a reply line written by [`encode_reply`].
pub fn decode_reply<T: DeserializeOwned>(line: &str) -> Result<Result<T, Failure>, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not JSON: {e}"))?;
    let Value::Object(mut map) = value else {
        return Err("not a JSON object".into());
    };
    let ok = match map.remove("ok") {
        Some(Value::Bool(ok)) => ok,
        _ => return Err("no `ok`".into()),
    };
    let rest = Value::Object(map);
    if ok {
        serde_json::from_value(rest)
            .map(Ok)
            .map_err(|e| e.to_string())
    } else {
        serde_json::from_value(rest)
            .map(Err)
            .map_err(|e| e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Errors

#[derive(Debug)]
pub enum VmError {
    Io {
        what: String,
        source: io::Error,
    },
    /// `vm.json` is unreadable or invalid.
    Config(String),
    /// `vibe-vm` answered with an error.
    Refused(Failure),
    /// `vibe-vm` answered with something that isn't the expected reply.
    Protocol(String),
    /// `vibe-vm` exited before it was ready.
    Exited(String),
    TimedOut(String),
}

impl VmError {
    /// The code `vibe-vm` refused with, if it did.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            VmError::Refused(f) => Some(f.code),
            _ => None,
        }
    }
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VmError::Io { what, source } => write!(f, "can't {what}: {source}"),
            VmError::Config(why) => write!(f, "bad VM configuration: {why}"),
            VmError::Refused(failure) => write!(f, "vibe-vm: {failure}"),
            VmError::Protocol(why) => write!(f, "unexpected answer from vibe-vm: {why}"),
            VmError::Exited(why) | VmError::TimedOut(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for VmError {}

fn io_err(what: impl Into<String>) -> impl FnOnce(io::Error) -> VmError {
    let what = what.into();
    move |source| VmError::Io { what, source }
}

// ---------------------------------------------------------------------------
// Starting vibe-vm

/// Runs `vibe-vm create` on `config`: gives the VM its machine identifier
/// and, if it has none, a MAC address, saves them into the file, and checks
/// Virtualization.framework accepts the VM, save and restore included.
pub fn create(vibe_vm: &Path, config: &Path) -> Result<Created, VmError> {
    let output = Command::new(vibe_vm)
        .arg("create")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .output()
        .map_err(io_err(format!("run {}", vibe_vm.display())))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().unwrap_or("");
    match decode_reply::<Created>(line) {
        Ok(Ok(created)) if output.status.success() => Ok(created),
        Ok(Err(failure)) => Err(VmError::Refused(failure)),
        _ => Err(VmError::Exited(format!(
            "vibe-vm create exited with {}: {}{}",
            output.status,
            stdout.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
    }
}

/// Where a `vibe-vm` serves and logs.
#[derive(Debug, Clone)]
pub struct Launch<'a> {
    /// The `vibe-vm` binary, signed with the virtualization entitlement.
    pub vibe_vm: &'a Path,
    /// The agent's `vm.json`, after [`create`].
    pub config: &'a Path,
    /// The control socket to serve, normally `run/<ref>.vm`.
    pub socket: &'a Path,
    /// `vibe-vm`'s log; appended to.
    pub log: &'a Path,
}

/// Boots the VM fresh in a detached `vibe-vm`.
pub fn boot(launch: &Launch<'_>) -> Result<Ready, VmError> {
    start(launch, "boot", START_TIMEOUT)
}

/// Restores the VM from its snapshot in a detached `vibe-vm`, and resumes
/// it. The snapshot is deleted once the VM runs again, since the VM's disks
/// move on from it. If it won't restore, it's kept and the error's code is
/// [`ErrorCode::RestoreFailed`].
pub fn restore(launch: &Launch<'_>) -> Result<Ready, VmError> {
    start(launch, "restore", START_TIMEOUT)
}

fn start(launch: &Launch<'_>, command: &str, timeout: Duration) -> Result<Ready, VmError> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(launch.log)
        .map_err(io_err(format!("open {}", launch.log.display())))?;
    let mut child = Command::new(launch.vibe_vm)
        .arg(command)
        .arg("--config")
        .arg(launch.config)
        .arg("--socket")
        .arg(launch.socket)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(log)
        .spawn()
        .map_err(io_err(format!("start {}", launch.vibe_vm.display())))?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read_line(&mut BufReader::new(stdout)));
    });
    let line = match rx.recv_timeout(timeout) {
        Ok(Ok(line)) => line,
        Ok(Err(e)) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VmError::Io {
                what: "read vibe-vm's ready line".into(),
                source: e,
            });
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(VmError::TimedOut(format!(
                "vibe-vm {command} wasn't ready in {}s; its log: {}",
                timeout.as_secs(),
                launch.log.display()
            )));
        }
    };
    if line.is_empty() {
        let status = child.wait().map_err(io_err("wait for vibe-vm"))?;
        return Err(VmError::Exited(format!(
            "vibe-vm {command} exited with {status} before it was ready; its log: {}",
            launch.log.display()
        )));
    }
    match decode_reply::<Ready>(&line) {
        Ok(Ok(ready)) => {
            reap(child);
            Ok(ready)
        }
        Ok(Err(failure)) => {
            let _ = child.wait();
            Err(VmError::Refused(failure))
        }
        Err(why) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(VmError::Protocol(format!("{why}: {line}")))
        }
    }
}

/// `vibe-vm` runs on after its ready line, detached in a session of its
/// own; this collects its exit so it never lingers as a zombie of `vibed`.
fn reap(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// One `\n`-terminated line, without the newline; empty at EOF. It reads a
/// byte at a time, so after a `connect` nothing of the guest's stream is
/// taken into a buffer.
fn read_line(reader: &mut impl Read) -> io::Result<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) => break,
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > MAX_LINE {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "a reply line is too long",
                    ));
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    String::from_utf8(line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ---------------------------------------------------------------------------
// The control socket

/// Sends `request` and reads the reply, leaving the stream just after it.
fn call<T: DeserializeOwned>(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<(T, UnixStream), VmError> {
    let mut stream =
        UnixStream::connect(socket).map_err(io_err(format!("connect to {}", socket.display())))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|()| stream.set_write_timeout(Some(CALL_TIMEOUT)))
        .map_err(io_err("set socket timeouts"))?;
    let mut line = serde_json::to_string(request).expect("a request always encodes");
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(io_err(format!("write to {}", socket.display())))?;
    let reply = read_line(&mut stream).map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => VmError::TimedOut(format!(
            "vibe-vm at {} didn't answer in {}s",
            socket.display(),
            timeout.as_secs()
        )),
        _ => VmError::Io {
            what: format!("read from {}", socket.display()),
            source: e,
        },
    })?;
    if reply.is_empty() {
        return Err(VmError::Protocol(
            "the connection closed without a reply".into(),
        ));
    }
    match decode_reply::<T>(&reply) {
        Ok(Ok(t)) => Ok((t, stream)),
        Ok(Err(failure)) => Err(VmError::Refused(failure)),
        Err(why) => Err(VmError::Protocol(format!("{why}: {reply}"))),
    }
}

pub fn status(socket: &Path) -> Result<Status, VmError> {
    call(socket, &Request::Status, CALL_TIMEOUT).map(|(s, _)| s)
}

/// Puts the VM to sleep: the reply comes once its snapshot is saved and the
/// VM stopped, and `vibe-vm` then exits. `vibe-guest` must have frozen the
/// agent first. `balloon_ms`: see [`Request::Sleep`].
pub fn sleep(socket: &Path, balloon_ms: Option<u64>) -> Result<Slept, VmError> {
    let timeout = SAVE_TIMEOUT + Duration::from_millis(balloon_ms.unwrap_or(10_000));
    call(socket, &Request::Sleep { balloon_ms }, timeout).map(|(s, _)| s)
}

/// Stops the VM without saving it; `vibe-vm` exits.
pub fn stop(socket: &Path) -> Result<(), VmError> {
    call::<Done>(socket, &Request::Stop, CALL_TIMEOUT).map(|_| ())
}

/// A stream to vsock `port` in the guest, through `vibe-vm`. It has no
/// timeouts set.
pub fn connect(socket: &Path, port: u32) -> Result<UnixStream, VmError> {
    let (Done {}, stream) = call(socket, &Request::Connect { port }, CALL_TIMEOUT)?;
    stream
        .set_read_timeout(None)
        .and_then(|()| stream.set_write_timeout(None))
        .map_err(io_err("clear socket timeouts"))?;
    Ok(stream)
}

/// What a VM's control socket says about it.
#[derive(Debug)]
pub enum Probe {
    Running(Status),
    /// No `vibe-vm` serves it: the VM is stopped or asleep.
    Gone,
    /// Something holds the socket's lock but doesn't answer as a `vibe-vm`
    /// does.
    Broken(String),
}

/// Asks the `vibe-vm` at `socket` for its status. As with shims, whether it
/// is gone is decided by its lock, which a `vibe-vm` holds for its whole
/// life, not by how the connect failed.
pub fn probe(socket: &Path) -> Probe {
    match status(socket) {
        Ok(status) => Probe::Running(status),
        Err(_) if !lock_held(socket) => Probe::Gone,
        Err(e) => Probe::Broken(e.to_string()),
    }
}

/// Whether a process holds the lock beside `socket`, as a running
/// `vibe-vm` does. If that can't be told, it's taken as held.
pub fn lock_held(socket: &Path) -> bool {
    let lock = match OpenOptions::new().write(true).open(lock_path(socket)) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return false,
        Err(_) => return true,
    };
    lock_taken(&lock)
}

fn lock_taken(lock: &File) -> bool {
    match lock.try_lock() {
        Ok(()) => false,
        Err(TryLockError::WouldBlock) => true,
        Err(TryLockError::Error(_)) => true,
    }
}

/// Waits up to `timeout` for the `vibe-vm` at `socket` to exit, as it does
/// after a sleep or a stop. True if it did.
pub fn wait_gone(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !lock_held(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    fn read_request(reader: &mut impl BufRead) -> io::Result<Option<Result<Request, String>>> {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        Ok(Some(
            serde_json::from_str(line.trim_end()).map_err(|e| e.to_string()),
        ))
    }

    fn config() -> VmConfig {
        let text = include_str!("../../../protocol/vibe-vm/config.json");
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn the_example_config_is_valid() {
        config().validate().unwrap();
    }

    type Change = Box<dyn Fn(&mut VmConfig)>;

    #[test]
    fn bad_configs_say_why() {
        let cases: Vec<(Change, &str)> = vec![
            (Box::new(|c| c.version = 2), "version 2 isn't supported"),
            (Box::new(|c| c.agent = "a/b".into()), "isn't a valid ref"),
            (Box::new(|c| c.cpus = 0), "cpus must be at least 1"),
            (Box::new(|c| c.memory = MIB + 1), "whole number of MiB"),
            (Box::new(|c| c.memory = 0), "whole number of MiB"),
            (
                Box::new(|c| c.kernel = "Image".into()),
                "kernel must be an absolute path",
            ),
            (
                Box::new(|c| c.initrd = Some("x".into())),
                "initrd must be an absolute path",
            ),
            (
                Box::new(|c| c.snapshot = "s".into()),
                "snapshot must be an absolute path",
            ),
            (
                Box::new(|c| c.console_log = Some("c".into())),
                "console_log must be an absolute path",
            ),
            (Box::new(|c| c.mac = Some("02:00".into())), "isn't a MAC"),
            (
                Box::new(|c| c.mac = Some("03:00:00:00:00:01".into())),
                "multicast",
            ),
            (
                Box::new(|c| c.machine_id = Some("not base64!".into())),
                "machine_id must be base64",
            ),
            (
                Box::new(|c| c.disks[1].id = "base".into()),
                "disk id `base` is used twice",
            ),
            (
                Box::new(|c| c.disks[0].id = "a".repeat(21)),
                "must be 1 to 20",
            ),
            (
                Box::new(|c| c.disks[0].id = String::new()),
                "must be 1 to 20",
            ),
            (
                Box::new(|c| c.disks[0].path = "base.img".into()),
                "disk `base` must be an absolute path",
            ),
            (
                Box::new(|c| c.shares[1].tag = "worktree".into()),
                "share tag `worktree` is used twice",
            ),
            (
                Box::new(|c| c.shares[0].tag = "a b".into()),
                "share tag `a b` must be",
            ),
            (
                Box::new(|c| c.shares[0].tag = "t".repeat(37)),
                "must be 1 to 36",
            ),
            (
                Box::new(|c| c.shares[0].path = "wt".into()),
                "share `worktree` must be an absolute path",
            ),
        ];
        for (change, expect) in cases {
            let mut c = config();
            change(&mut c);
            let err = c.validate().unwrap_err();
            assert!(err.contains(expect), "{err} should contain {expect}");
        }
        // The limits themselves are fine.
        let mut c = config();
        c.disks[0].id = "a".repeat(20);
        c.shares[0].tag = "t".repeat(36);
        c.mac = None;
        c.machine_id = None;
        c.initrd = None;
        c.console_log = None;
        c.validate().unwrap();
    }

    #[test]
    fn macs() {
        assert_eq!(
            parse_mac("02:5f:3a:11:9c:e4"),
            Ok([2, 0x5f, 0x3a, 0x11, 0x9c, 0xe4])
        );
        assert_eq!(parse_mac("02:5F:3A:11:9C:E4").unwrap()[1], 0x5f);
        for bad in [
            "",
            "02:5f:3a:11:9c",
            "02:5f:3a:11:9c:e4:00",
            "2:5f:3a:11:9c:e4",
            "02-5f-3a-11-9c-e4",
            "zz:5f:3a:11:9c:e4",
        ] {
            assert!(parse_mac(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn vcpus_stay_within_the_host() {
        assert_eq!(vcpus(4, 10), 4);
        assert_eq!(vcpus(16, 10), 10);
        assert_eq!(vcpus(0, 10), 1);
        assert_eq!(vcpus(4, 0), 1);
    }

    #[test]
    fn configs_save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vm.json");
        let c = config();
        c.save(&path).unwrap();
        assert_eq!(VmConfig::load(&path).unwrap(), c);
        let mut bad = c.clone();
        bad.cpus = 0;
        assert!(matches!(bad.save(&path), Err(VmError::Config(_))));
        // A failed save leaves the old file.
        assert_eq!(VmConfig::load(&path).unwrap(), c);
        fs::write(&path, "{").unwrap();
        let err = VmConfig::load(&path).unwrap_err();
        assert!(matches!(err, VmError::Config(_)), "{err}");
        let leftovers: Vec<_> = fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(leftovers.len(), 1);
    }

    #[test]
    fn disks_are_sparse_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.img");
        create_disk(&path, 1 << 30).unwrap();
        let meta = fs::metadata(&path).unwrap();
        assert_eq!(meta.len(), 1 << 30);
        use std::os::unix::fs::MetadataExt;
        assert!(meta.blocks() * 512 < 1 << 20, "not sparse");
        assert!(create_disk(&path, 1 << 20).is_err());
        assert_eq!(fs::metadata(&path).unwrap().len(), 1 << 30);
    }

    #[test]
    fn replies_round_trip() {
        let ok: Result<Done, Failure> = Ok(Done {});
        assert_eq!(encode_reply(&ok), r#"{"ok":true}"#);
        let err: Result<Done, Failure> = Err(Failure {
            code: ErrorCode::Busy,
            error: "x".into(),
        });
        let line = encode_reply(&err);
        assert_eq!(line, r#"{"code":"busy","error":"x","ok":false}"#);
        assert_eq!(decode_reply::<Done>(&line).unwrap(), err);
        for bad in ["", "[]", "{}", r#"{"ok":1}"#, r#"{"ok":true,"pid":"x"}"#] {
            assert!(decode_reply::<Status>(bad).is_err(), "{bad}");
        }
    }

    // A stand-in for vibe-vm's control socket: it holds the lock, answers
    // each connection with the next canned reply, and after a `connect`
    // echoes the stream.
    struct FakeVm {
        socket: PathBuf,
        _dir: tempfile::TempDir,
        requests: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
        lock: Option<File>,
    }

    impl FakeVm {
        fn serve(replies: Vec<String>) -> FakeVm {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("x.vm");
            let lock = File::create(lock_path(&socket)).unwrap();
            lock.lock().unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = requests.clone();
            std::thread::spawn(move || {
                for (stream, reply) in listener.incoming().zip(replies) {
                    let mut stream = stream.unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let Some(Ok(request)) = read_request(&mut reader).unwrap() else {
                        continue;
                    };
                    seen.lock().unwrap().push(request.clone());
                    stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
                    if matches!(request, Request::Connect { .. }) && reply.contains("true") {
                        // Guest bytes may follow the reply at once.
                        stream.write_all(b"hello from the guest\n").unwrap();
                        let mut buf = [0u8; 256];
                        loop {
                            match reader.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => stream.write_all(&buf[..n]).unwrap(),
                            }
                        }
                    }
                }
            });
            FakeVm {
                socket,
                _dir: dir,
                requests,
                lock: Some(lock),
            }
        }
    }

    #[test]
    fn calls_send_requests_and_read_replies() {
        let vm = FakeVm::serve(vec![
            r#"{"ok":true,"protocol":1,"ref":"x","pid":7,"state":"running","started":"boot"}"#
                .into(),
            r#"{"ok":true,"snapshot":"/s/x","snapshot_bytes":5,"timings":{"save_ms":9}}"#.into(),
            r#"{"ok":false,"code":"bad_state","error":"the VM is paused"}"#.into(),
            r#"{"ok":true}"#.into(),
            r#"{"ok":true,"pid":"not a status"}"#.into(),
        ]);
        let s = status(&vm.socket).unwrap();
        assert_eq!(
            (s.agent.as_str(), s.pid, s.state),
            ("x", 7, VmState::Running)
        );
        let slept = sleep(&vm.socket, Some(0)).unwrap();
        assert_eq!(slept.snapshot_bytes, 5);
        assert_eq!(slept.timings.save_ms, Some(9));
        let err = stop(&vm.socket).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::BadState));
        assert_eq!(err.to_string(), "vibe-vm: the VM is paused");
        stop(&vm.socket).unwrap();
        assert!(matches!(status(&vm.socket), Err(VmError::Protocol(_))));
        assert_eq!(
            *vm.requests.lock().unwrap(),
            [
                Request::Status,
                Request::Sleep {
                    balloon_ms: Some(0)
                },
                Request::Stop,
                Request::Stop,
                Request::Status
            ]
        );
    }

    #[test]
    fn connect_hands_over_the_stream_intact() {
        let vm = FakeVm::serve(vec![
            r#"{"ok":true}"#.into(),
            r#"{"ok":false,"code":"connect_failed","error":"nothing on port 9"}"#.into(),
        ]);
        let mut stream = connect(&vm.socket, 1024).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "hello from the guest\n");
        stream.write_all(b"ping\n").unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line, "ping\n");
        drop((stream, reader));
        let err = connect(&vm.socket, 9).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::ConnectFailed));
    }

    #[test]
    fn probe_tells_running_gone_and_broken_apart() {
        let mut vm = FakeVm::serve(vec![
            r#"{"ok":true,"protocol":1,"ref":"x","pid":7,"state":"paused","started":"restore"}"#
                .into(),
            "garbage".into(),
        ]);
        assert!(matches!(probe(&vm.socket), Probe::Running(s) if s.state == VmState::Paused));
        assert!(matches!(probe(&vm.socket), Probe::Broken(_)));
        assert!(lock_held(&vm.socket));
        assert!(!wait_gone(&vm.socket, Duration::from_millis(50)));
        // Its process exits: the lock goes with it.
        vm.lock = None;
        assert!(wait_gone(&vm.socket, Duration::from_secs(5)));
        assert!(matches!(probe(&vm.socket), Probe::Gone));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(probe(&dir.path().join("none.vm")), Probe::Gone));
    }

    // Fake vibe-vm binaries: shell scripts that print what a real one would.
    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("vibe-vm");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn launch<'a>(dir: &'a Path, bin: &'a Path, log: &'a Path) -> Launch<'a> {
        Launch {
            vibe_vm: bin,
            config: dir,
            socket: dir,
            log,
        }
    }

    #[test]
    fn start_reads_the_ready_line_and_leaves_vibe_vm_running() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("args");
        let bin = script(
            dir.path(),
            &format!(
                r#"echo "$@" > {m}
echo starting >&2
echo '{{"ok":true,"protocol":1,"pid":'$$',"started":"restore","timings":{{"restore_ms":400,"resume_ms":2,"total_ms":450}}}}'
exec sleep 30"#,
                m = marker.display()
            ),
        );
        let log = dir.path().join("vm.log");
        let ready = restore(&launch(dir.path(), &bin, &log)).unwrap();
        assert_eq!(ready.started, Started::Restore);
        assert_eq!(ready.timings.restore_ms, Some(400));
        let args = fs::read_to_string(&marker).unwrap();
        let d = dir.path().display();
        assert_eq!(args.trim(), format!("restore --config {d} --socket {d}"));
        assert_eq!(fs::read_to_string(&log).unwrap(), "starting\n");
        // It's still running, as a VM would be.
        let pid = ready.pid as i32;
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }

    #[test]
    fn start_reports_failures() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("vm.log");

        let bin = script(
            dir.path(),
            r#"echo '{"ok":false,"code":"restore_failed","error":"won'"'"'t restore"}'; exit 1"#,
        );
        let err = restore(&launch(dir.path(), &bin, &log)).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::RestoreFailed));

        let bin = script(dir.path(), "echo boom >&2; exit 3");
        let err = boot(&launch(dir.path(), &bin, &log)).unwrap_err();
        assert!(matches!(err, VmError::Exited(_)), "{err}");
        assert!(
            err.to_string().contains("exited with exit status: 3"),
            "{err}"
        );

        let bin = script(dir.path(), "echo not json; exec sleep 30");
        let err = boot(&launch(dir.path(), &bin, &log)).unwrap_err();
        assert!(matches!(err, VmError::Protocol(_)), "{err}");

        let bin = script(dir.path(), "exec sleep 30");
        let err = start(
            &launch(dir.path(), &bin, &log),
            "boot",
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert!(matches!(err, VmError::TimedOut(_)), "{err}");

        let missing = dir.path().join("missing");
        let err = boot(&launch(dir.path(), &missing, &log)).unwrap_err();
        assert!(matches!(err, VmError::Io { .. }), "{err}");
    }

    #[test]
    fn create_reads_the_identity() {
        let dir = tempfile::tempdir().unwrap();
        let bin = script(
            dir.path(),
            r#"[ "$1 $2" = "create --config" ] || exit 9
echo '{"ok":true,"machine_id":"AAAA","mac":"02:00:00:00:00:01"}'"#,
        );
        let created = create(&bin, dir.path()).unwrap();
        assert_eq!(created.mac, "02:00:00:00:00:01");

        let bin = script(
            dir.path(),
            r#"echo '{"ok":false,"code":"unsupported","error":"no"}'; exit 1"#,
        );
        let err = create(&bin, dir.path()).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::Unsupported));

        let bin = script(dir.path(), "echo usage >&2; exit 2");
        let err = create(&bin, dir.path()).unwrap_err();
        assert!(err.to_string().contains("usage"), "{err}");
    }
}
