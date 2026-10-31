//! The two ports: the control channel and the terminal (`vibe_guest::proto`).
//! Each connection gets a thread of its own.

use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;
use vibe_guest::proto::{
    self, ClockSet, Done, ErrorCode, Failure, FrameKind, Network, PROTOCOL, Request, Sealed,
    ShareMount, Status, TermHello, Thawed, read_frame,
};

use crate::init::root::{self, Writable};
use crate::init::session::{Agent, Conn, lock, refusal, set_timeouts};
use crate::init::{log, net, sys};

/// How long a control client has to send its request and read the reply.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a shim has to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// How long one write to a shim may block; a whole frame gets a few
/// seconds more (`session::Conn`).
const SHIM_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long a poweroff gives the agent to end before it's killed.
const POWEROFF_GRACE: Duration = Duration::from_secs(3);

/// Everything the ports answer from.
#[derive(Debug)]
pub(crate) struct Guest {
    pub(crate) agent: Arc<Agent>,
    /// PID 1 of a VM, rather than a test's process.
    pub(crate) pid1: bool,
    pub(crate) layers: Vec<String>,
    /// The writable disks' filesystems, which a seal freezes.
    pub(crate) writable: Vec<Writable>,
    /// Whether `vibe` may use `sudo`, until the seal.
    pub(crate) sudo: bool,
    /// Set once sealed: what the seal said. Nothing runs after it.
    pub(crate) sealed: Mutex<Option<Sealed>>,
    pub(crate) shares: Vec<ShareMount>,
    pub(crate) problems: Mutex<Vec<String>>,
    pub(crate) network: Arc<Mutex<Option<Network>>>,
}

/// A vsock listener, or a Unix one standing in for it in tests.
#[derive(Debug)]
pub(crate) enum Listener {
    Vsock(OwnedFd),
    Unix(UnixListener),
}

impl Listener {
    pub(crate) fn bind(port: u32, unix_dir: Option<&Path>) -> io::Result<Listener> {
        if let Some(dir) = unix_dir {
            let path = dir.join(port.to_string());
            let _ = fs::remove_file(&path);
            return UnixListener::bind(&path).map(Listener::Unix);
        }
        // SAFETY: plain syscalls on a descriptor this function owns.
        unsafe {
            let fd = sys::check(libc::socket(
                libc::AF_VSOCK,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            ))?;
            let owned = OwnedFd::from_raw_fd(fd);
            let mut addr: libc::sockaddr_vm = std::mem::zeroed();
            addr.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            addr.svm_port = port;
            addr.svm_cid = libc::VMADDR_CID_ANY;
            sys::check(libc::bind(
                fd,
                (&raw const addr).cast(),
                size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            ))?;
            sys::check(libc::listen(fd, 16))?;
            Ok(Listener::Vsock(owned))
        }
    }

    fn accept(&self) -> io::Result<File> {
        match self {
            Listener::Unix(l) => l.accept().map(|(s, _)| File::from(OwnedFd::from(s))),
            Listener::Vsock(fd) => {
                // SAFETY: a zeroed sockaddr_vm is valid for accept to fill.
                let mut peer: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
                let mut len = size_of::<libc::sockaddr_vm>() as libc::socklen_t;
                // SAFETY: accept on a listening socket, with an address of
                // the size given; the result is owned at once.
                let conn = unsafe {
                    libc::accept4(
                        fd.as_raw_fd(),
                        (&raw mut peer).cast(),
                        &mut len,
                        libc::SOCK_CLOEXEC,
                    )
                };
                if conn < 0 {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: a new descriptor nothing else owns.
                let conn = File::from(unsafe { OwnedFd::from_raw_fd(conn) });
                // Only the host may drive the guest: never a process in it,
                // the agent's own included, through vsock's loopback.
                if peer.svm_cid != libc::VMADDR_CID_HOST {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        format!("refused a connection from vsock cid {}", peer.svm_cid),
                    ));
                }
                Ok(conn)
            }
        }
    }
}

/// Binds `port`, trying again every second until it can: vsock may not be
/// there yet in a guest still loading its drivers.
pub(crate) fn bind_patiently(port: u32, unix_dir: Option<PathBuf>) -> Listener {
    let mut said = false;
    loop {
        match Listener::bind(port, unix_dir.as_deref()) {
            Ok(l) => {
                log!("serving port {port}");
                return l;
            }
            Err(e) => {
                if !said {
                    log!("can't serve port {port} yet: {e}; trying every second");
                    said = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
}

/// Accepts connections for ever, each handled on a thread by `handle`.
pub(crate) fn serve(
    guest: Arc<Guest>,
    listener: Listener,
    name: &'static str,
    handle: fn(&Guest, File),
) -> ! {
    loop {
        match listener.accept() {
            Ok(conn) => {
                let guest = guest.clone();
                if let Err(e) = std::thread::Builder::new()
                    .name(name.into())
                    .spawn(move || handle(&guest, conn))
                {
                    log!("can't start a thread for a connection: {e}");
                }
            }
            Err(e) => {
                log!("accept on the {name} port: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The control channel

pub(crate) fn control(guest: &Guest, conn: File) {
    if let Err(e) = set_timeouts(
        conn.as_raw_fd(),
        Some(CONTROL_TIMEOUT),
        Some(CONTROL_TIMEOUT),
    ) {
        log!("control connection: {e}");
        return;
    }
    let mut line = Vec::new();
    let read =
        BufReader::new((&conn).take(proto::MAX_LINE as u64 + 1)).read_until(b'\n', &mut line);
    if read.is_err() || line.is_empty() {
        return;
    }
    let (reply, then) = match std::str::from_utf8(&line) {
        Ok(text) => match proto::decode_request(text.trim_end()) {
            Ok(request) => answer(guest, request),
            Err(f) => (Err(f), None),
        },
        Err(_) => (
            Err(Failure::new(
                ErrorCode::BadRequest,
                "the request isn't UTF-8",
            )),
            None,
        ),
    };
    let mut out = proto::encode_reply(&reply);
    out.push('\n');
    let _ = (&conn).write_all(out.as_bytes());
    drop(conn);
    if let Some(then) = then {
        then(guest);
    }
}

type Then = fn(&Guest);

fn json<T: serde::Serialize>(value: T) -> Result<Value, Failure> {
    Ok(serde_json::to_value(value).expect("replies serialize to JSON"))
}

/// The reply to a request, and what to do once it's sent.
fn answer(guest: &Guest, request: Request) -> (Result<Value, Failure>, Option<Then>) {
    let op = match &request {
        Request::Status => "status",
        Request::Launch(_) => "launch",
        Request::Processes => "processes",
        Request::Freeze => "freeze",
        Request::Thaw { .. } => "thaw",
        Request::Clock { .. } => "clock",
        Request::Poweroff => "poweroff",
        Request::Run(_) => "run",
        Request::Seal => "seal",
    };
    if op != "status" && op != "processes" {
        log!("control: {op}");
    }
    // A sealed guest's disks are frozen: anything that ran would hang on
    // its first write.
    if matches!(request, Request::Launch(_) | Request::Run(_)) && lock(&guest.sealed).is_some() {
        let f = Failure::new(
            ErrorCode::Busy,
            "the guest is sealed: only a poweroff is left",
        );
        log!("control: {op} failed: {f}");
        return (Err(f), None);
    }
    let reply = match request {
        Request::Status => json(status(guest)),
        Request::Launch(launch) => guest.agent.launch(&launch).and_then(json),
        Request::Processes => json(guest.agent.processes()),
        Request::Freeze => guest.agent.freeze().and_then(json),
        Request::Thaw {
            time_ms,
            share_timeout_ms,
        } => thaw(guest, time_ms, share_timeout_ms).and_then(json),
        Request::Clock { time_ms } => sys::set_clock(time_ms)
            .map(|step_ms| ClockSet { step_ms })
            .map_err(|e| Failure::new(ErrorCode::ClockFailed, format!("can't set the clock: {e}")))
            .and_then(json),
        Request::Poweroff => return (json(Done {}), Some(poweroff)),
        Request::Run(run) => guest.agent.run(&run).and_then(json),
        Request::Seal => seal(guest).and_then(json),
    };
    if let Err(f) = &reply {
        log!("control: {op} failed: {f}");
    }
    (reply, None)
}

fn status(guest: &Guest) -> Status {
    let (state, frozen, session) = guest.agent.status();
    Status {
        protocol: PROTOCOL,
        version: env!("CARGO_PKG_VERSION").into(),
        boot_id: sys::boot_id(),
        uptime_ms: sys::uptime_ms(),
        state,
        frozen,
        session,
        memory: sys::memory(),
        network: lock(&guest.network).clone(),
        layers: guest.layers.clone(),
        shares: guest.shares.clone(),
        problems: lock(&guest.problems).clone(),
    }
}

/// After a restore, in this order: the clock, which stopped while the VM
/// slept; the shares, which don't answer for about a second; then the
/// agent. It always thaws.
fn thaw(guest: &Guest, time_ms: u64, timeout_ms: Option<u64>) -> Result<Thawed, Failure> {
    let mut problems = Vec::new();
    let clock_step_ms = match sys::set_clock(time_ms) {
        Ok(step) => Some(step),
        Err(e) => {
            problems.push(format!("can't set the clock: {e}"));
            None
        }
    };
    let t = Instant::now();
    let timeout = Duration::from_millis(timeout_ms.unwrap_or(proto::SHARE_TIMEOUT_MS));
    if guest.pid1 {
        // Forget the dentries and inodes no one holds, so what's looked up
        // next is asked of the host afresh, not of names from before.
        let _ = fs::write("/proc/sys/vm/drop_caches", "2");
    }
    let unanswered = wait_for_shares(&guest.shares, timeout);
    let shares_ms = t.elapsed().as_millis() as u64;
    if !unanswered.is_empty() {
        problems.push(format!(
            "shares still not answering after {} ms: {}",
            shares_ms,
            unanswered.join(", ")
        ));
    }
    if guest.pid1 {
        // The lease's clock stopped too; the NAT may have moved on.
        net::renew_soon();
    }
    let thaw_ms = guest.agent.thaw().map_err(|f| {
        Failure::new(
            f.code,
            format!(
                "{} (the clock moved {clock_step_ms:?} ms; shares took {shares_ms} ms{})",
                f.error,
                if unanswered.is_empty() {
                    String::new()
                } else {
                    format!(", {} never answered", unanswered.join(", "))
                }
            ),
        )
    })?;
    log!("after the restore: the clock moved {clock_step_ms:?} ms, the shares took {shares_ms} ms");
    Ok(Thawed {
        clock_step_ms,
        shares_ms,
        unanswered,
        thaw_ms,
        problems,
    })
}

/// Waits for every share to answer: its root lists, and every name in it
/// can be looked up. After a restore both fail for a moment. The tags of
/// those still failing when `timeout` runs out.
fn wait_for_shares(shares: &[ShareMount], timeout: Duration) -> Vec<String> {
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    for share in shares {
        let (tx, share) = (tx.clone(), share.clone());
        // A share that hangs rather than fails mustn't hold up the rest,
        // so each is probed on a thread of its own, left behind if it does.
        let _ = std::thread::Builder::new()
            .name("share".into())
            .spawn(move || {
                loop {
                    if answers(Path::new(&share.path)) {
                        let _ = tx.send(share.tag);
                        return;
                    }
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
            });
    }
    drop(tx);
    let mut answered = Vec::new();
    while answered.len() < shares.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left + Duration::from_millis(50)) {
            Ok(tag) => answered.push(tag),
            Err(_) => break,
        }
    }
    shares
        .iter()
        .filter(|s| !answered.contains(&s.tag))
        .map(|s| s.tag.clone())
        .collect()
}

fn answers(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .take(64)
        .all(|e| e.is_ok_and(|e| fs::symlink_metadata(e.path()).is_ok()))
}

/// Ends the command, cleans up what the toolbox shouldn't keep, flushes,
/// and freezes every writable disk's filesystem, so each is left clean for
/// the host to keep. Sealing twice says the same again.
fn seal(guest: &Guest) -> Result<Sealed, Failure> {
    let mut sealed = lock(&guest.sealed);
    if let Some(s) = &*sealed {
        return Ok(s.clone());
    }
    let t = Instant::now();
    guest.agent.end_all(POWEROFF_GRACE);
    if guest.pid1 {
        if guest.sudo
            && let Err(e) = root::set_sudo(false)
        {
            return Err(Failure::new(
                ErrorCode::SealFailed,
                format!("can't take sudo away again: {e}"),
            ));
        }
        // What installers leave in /tmp isn't for every agent to boot with.
        if let Ok(entries) = fs::read_dir("/tmp") {
            for e in entries.flatten() {
                let path = e.path();
                let gone = match e.file_type() {
                    Ok(t) if t.is_dir() => fs::remove_dir_all(&path),
                    _ => fs::remove_file(&path),
                };
                if let Err(err) = gone {
                    log!("seal: can't remove {}: {err}", path.display());
                }
            }
        }
    }
    // SAFETY: sync takes no arguments and can't fail.
    unsafe { libc::sync() };
    let mut disks = Vec::new();
    for w in &guest.writable {
        root::freeze_filesystem(&w.dir).map_err(|e| {
            Failure::new(ErrorCode::SealFailed, format!("can't freeze {}: {e}", w.id))
        })?;
        disks.push(w.id.clone());
    }
    let done = Sealed {
        disks,
        seal_ms: t.elapsed().as_millis() as u64,
    };
    log!("sealed {:?} in {} ms", done.disks, done.seal_ms);
    *sealed = Some(done.clone());
    Ok(done)
}

fn poweroff(guest: &Guest) {
    log!("powering off");
    guest.agent.end_all(POWEROFF_GRACE);
    // A sealed guest's disks are flushed and frozen already.
    if lock(&guest.sealed).is_none() {
        // SAFETY: sync takes no arguments and can't fail.
        unsafe { libc::sync() };
    }
    if guest.pid1 {
        // SAFETY: as PID 1, this powers the VM off; nothing returns.
        unsafe { libc::reboot(libc::RB_POWER_OFF) };
        log!("the poweroff failed: {}", io::Error::last_os_error());
    } else {
        std::process::exit(0);
    }
}

// ---------------------------------------------------------------------------
// The terminal

pub(crate) fn terminal(guest: &Guest, conn: File) {
    let fd = conn.as_raw_fd();
    if let Err(e) = set_timeouts(fd, Some(HELLO_TIMEOUT), Some(SHIM_WRITE_TIMEOUT)) {
        log!("terminal connection: {e}");
        return;
    }
    let mut reader = &conn;
    let hello = match read_frame(&mut reader) {
        Ok(Some(f)) if f.kind == FrameKind::Hello => {
            // The version first, so a newer shim whose hello has fields
            // this guest doesn't know still learns which it speaks.
            let theirs = f
                .decode::<Value>()
                .ok()
                .and_then(|v| v.get("protocol").and_then(Value::as_u64));
            match theirs {
                Some(p) if p != u64::from(PROTOCOL) => {
                    Err(format!("this guest speaks protocol {PROTOCOL}, not {p}"))
                }
                _ => f.decode::<TermHello>().map_err(|e| e.to_string()),
            }
        }
        Ok(Some(f)) => Err(format!("a {:?} frame before hello", f.kind)),
        Ok(None) => return,
        Err(e) => Err(format!("expected a hello: {e}")),
    };
    let hello = match hello {
        Ok(h) => h,
        Err(message) => {
            log!("refused a terminal client: {message}");
            let _ = proto::write_frame(&mut &conn, &refusal(message));
            return;
        }
    };
    let id = guest.agent.next_conn_id();
    let stream = match conn.try_clone() {
        Ok(s) => s,
        Err(e) => {
            log!("terminal connection: {e}");
            return;
        }
    };
    let shared = Arc::new(Conn::new(id, hello.client.clone(), stream));
    match guest.agent.attach(shared, hello.size) {
        Ok(a) if a.live => {}
        Ok(_) => return,
        Err(message) => {
            log!("refused {}: {message}", hello.client);
            let _ = proto::write_frame(&mut &conn, &refusal(message));
            return;
        }
    }
    // Attached: input may be a long time coming.
    let _ = set_timeouts(fd, None, Some(SHIM_WRITE_TIMEOUT));
    loop {
        match read_frame(&mut reader) {
            Ok(Some(f)) => match f.kind {
                FrameKind::Input => {
                    if let Err(e) = guest.agent.input(id, &f.payload) {
                        log!("can't write to the terminal: {e}");
                    }
                }
                FrameKind::Resize => match f.decode() {
                    Ok(size) => guest.agent.resize(id, size),
                    Err(e) => {
                        log!("{}: {e}", hello.client);
                        break;
                    }
                },
                other => {
                    log!("{} sent a {other:?} frame; closing", hello.client);
                    break;
                }
            },
            Ok(None) => break,
            Err(e) => {
                if !matches!(
                    e.kind(),
                    io::ErrorKind::ConnectionReset | io::ErrorKind::BrokenPipe
                ) {
                    log!("{} (connection {id}): {e}", hello.client);
                }
                break;
            }
        }
    }
    guest.agent.detach(id);
}
