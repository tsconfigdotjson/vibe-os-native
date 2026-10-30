//! Driving `vibe-guest`, PID 1 inside a `linux` agent's VM (PRD: What's
//! inside `linux`, Sleep and wake). Only `vibe-vm` can reach the VM's vsock,
//! so every call goes through its control socket's `connect` ([`vm`]); the
//! contract is in `protocol/vibe-guest/`, and its types are
//! `vibe_guest::proto`, re-exported here.
//!
//! `vibed` uses it in this order:
//!
//! 1. After a boot: [`wait_ready`], then [`launch`] Claude with the token
//!    in its environment, and start a shim on the terminal
//!    (`vibe-shim --vm run/<ref>.vm`), which uses [`attach`].
//! 2. For the idle rule: [`processes`].
//! 3. To sleep: [`freeze`], then `vm::sleep` with [`balloon_floor`] of what
//!    the freeze reports.
//! 4. After `vm::restore`: [`thaw`] with the host's clock, then a new shim.

use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::vm::{self, VmError};

pub use vibe_guest::boot;
pub use vibe_guest::proto::{
    self, ClockSet, Done, ErrorCode, Exit, Failure, Frozen, GuestState, Launch, Launched, Memory,
    Process, Processes, Request, Session, Status, TermSize, TermWelcome, Thawed,
};

/// How long the guest may take to answer anything but a thaw.
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// How long [`wait_ready`] waits between tries.
const POLL: Duration = Duration::from_millis(50);

/// The least headroom the balloon leaves the guest above what it uses.
const MIN_HEADROOM: u64 = 256 << 20;

#[derive(Debug)]
pub enum GuestError {
    /// `vibe-vm` couldn't connect to the guest.
    Vm(VmError),
    Io {
        what: String,
        source: io::Error,
    },
    /// The guest answered with an error.
    Refused(Failure),
    /// The guest answered with something that isn't the expected reply.
    Protocol(String),
}

impl GuestError {
    /// The code the guest refused with, if it did.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            GuestError::Refused(f) => Some(f.code),
            _ => None,
        }
    }
}

impl fmt::Display for GuestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuestError::Vm(e) => write!(f, "can't reach vibe-guest: {e}"),
            GuestError::Io { what, source } => write!(f, "can't {what}: {source}"),
            GuestError::Refused(failure) => write!(f, "vibe-guest: {failure}"),
            GuestError::Protocol(why) => write!(f, "unexpected answer from vibe-guest: {why}"),
        }
    }
}

impl std::error::Error for GuestError {}

fn io_err(what: &'static str) -> impl FnOnce(io::Error) -> GuestError {
    move |source| GuestError::Io {
        what: what.into(),
        source,
    }
}

/// One request on the control port, through the `vibe-vm` at `socket`.
fn call<T: DeserializeOwned>(
    socket: &Path,
    request: &Request,
    timeout: Duration,
) -> Result<T, GuestError> {
    let mut stream =
        vm::connect_within(socket, proto::CONTROL_PORT, timeout).map_err(GuestError::Vm)?;
    vm::set_timeouts(&stream, Some(timeout), Some(CALL_TIMEOUT))
        .map_err(io_err("set socket timeouts"))?;
    let mut line = serde_json::to_string(request).expect("a request always encodes");
    line.push('\n');
    stream
        .write_all(line.as_bytes())
        .map_err(io_err("write to vibe-guest"))?;
    let mut reply = String::new();
    BufReader::new(stream)
        .take(proto::MAX_LINE as u64)
        .read_line(&mut reply)
        .map_err(io_err("read from vibe-guest"))?;
    if reply.is_empty() {
        return Err(GuestError::Protocol(
            "the connection closed without a reply".into(),
        ));
    }
    match proto::decode_reply::<T>(reply.trim_end()) {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(failure)) => Err(GuestError::Refused(failure)),
        Err(why) => Err(GuestError::Protocol(format!("{why}: {}", reply.trim_end()))),
    }
}

pub fn status(socket: &Path) -> Result<Status, GuestError> {
    let status: Status = call(socket, &Request::Status, CALL_TIMEOUT)?;
    if status.protocol != proto::PROTOCOL {
        return Err(GuestError::Protocol(format!(
            "vibe-guest speaks protocol {}, not {}",
            status.protocol,
            proto::PROTOCOL
        )));
    }
    Ok(status)
}

/// Waits for a guest that's booting or being restored to answer, up to
/// `timeout`. Until then a connect may fail, or be accepted and dropped.
pub fn wait_ready(socket: &Path, timeout: Duration) -> Result<Status, GuestError> {
    let deadline = Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let tried = vm::connect_within(
            socket,
            proto::CONTROL_PORT,
            left.min(Duration::from_secs(2)),
        )
        .map_err(GuestError::Vm)
        .and_then(|_| status(socket));
        match tried {
            Ok(status) => return Ok(status),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(POLL),
        }
    }
}

/// Starts the agent's command. The environment goes to it alone: the guest
/// never writes it down, so it's where the Claude token goes.
pub fn launch(socket: &Path, launch: &Launch) -> Result<Launched, GuestError> {
    launch
        .validate()
        .map_err(|e| GuestError::Refused(Failure::new(ErrorCode::BadRequest, e)))?;
    call(socket, &Request::Launch(launch.clone()), CALL_TIMEOUT)
}

pub fn processes(socket: &Path) -> Result<Processes, GuestError> {
    call(socket, &Request::Processes, CALL_TIMEOUT)
}

/// Freezes the agent and flushes the guest's disks and shares: before a
/// sleep.
pub fn freeze(socket: &Path) -> Result<Frozen, GuestError> {
    call(socket, &Request::Freeze, CALL_TIMEOUT)
}

/// After a restore: sets the guest's clock to `now`, waits for the shares
/// (up to `share_timeout`, or the guest's default), then thaws the agent.
pub fn thaw(
    socket: &Path,
    now: SystemTime,
    share_timeout: Option<Duration>,
) -> Result<Thawed, GuestError> {
    let wait = share_timeout.unwrap_or(Duration::from_millis(proto::SHARE_TIMEOUT_MS));
    call(
        socket,
        &Request::Thaw {
            time_ms: millis(now),
            share_timeout_ms: share_timeout.map(|d| d.as_millis() as u64),
        },
        CALL_TIMEOUT + wait,
    )
}

pub fn set_clock(socket: &Path, now: SystemTime) -> Result<ClockSet, GuestError> {
    call(
        socket,
        &Request::Clock {
            time_ms: millis(now),
        },
        CALL_TIMEOUT,
    )
}

/// Ends the agent, flushes the disks and powers the VM off; `vibe-vm` then
/// exits.
pub fn poweroff(socket: &Path) -> Result<(), GuestError> {
    call::<Done>(socket, &Request::Poweroff, CALL_TIMEOUT).map(drop)
}

/// Milliseconds since the Unix epoch, as the guest's clock takes them.
pub fn millis(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

/// How low the balloon may take a guest that uses `memory` (PRD: Sleep and
/// wake): what it uses, plus a quarter of that or 256 MiB, whichever is
/// more, so nothing in it runs short while the balloon is up.
pub fn balloon_floor(memory: Memory) -> u64 {
    let used = memory.total.saturating_sub(memory.available);
    (used + (used / 4).max(MIN_HEADROOM)).min(memory.total)
}

/// A shim's connection to the agent's terminal, past the welcome. The
/// screen follows as the first output frame.
#[derive(Debug)]
pub struct Terminal {
    pub stream: UnixStream,
    pub welcome: TermWelcome,
}

/// Attaches to the agent's terminal as `client`, taking over from any shim
/// attached before. With `size`, the terminal is that size before the
/// screen is sent.
pub fn attach(socket: &Path, client: &str, size: Option<TermSize>) -> Result<Terminal, GuestError> {
    let mut stream =
        vm::connect_within(socket, proto::TERMINAL_PORT, CALL_TIMEOUT).map_err(GuestError::Vm)?;
    vm::set_timeouts(&stream, Some(CALL_TIMEOUT), Some(CALL_TIMEOUT))
        .map_err(io_err("set socket timeouts"))?;
    let hello = proto::TermHello {
        protocol: proto::PROTOCOL,
        client: client.into(),
        size,
    };
    send(&mut stream, proto::FrameKind::Hello, &hello)?;
    let frame = proto::read_frame(&mut stream)
        .map_err(io_err("read from vibe-guest"))?
        .ok_or_else(|| GuestError::Protocol("the guest closed the terminal".into()))?;
    let decode = |e: io::Error| GuestError::Protocol(e.to_string());
    match frame.kind {
        proto::FrameKind::Welcome => {
            vm::set_timeouts(&stream, None, None).map_err(io_err("clear socket timeouts"))?;
            Ok(Terminal {
                welcome: frame.decode().map_err(decode)?,
                stream,
            })
        }
        proto::FrameKind::Refused => {
            let refused: proto::Refused = frame.decode().map_err(decode)?;
            Err(GuestError::Refused(Failure::new(
                ErrorCode::BadRequest,
                refused.message,
            )))
        }
        other => Err(GuestError::Protocol(format!(
            "a {other:?} frame instead of a welcome"
        ))),
    }
}

fn send(
    stream: &mut UnixStream,
    kind: proto::FrameKind,
    value: &impl Serialize,
) -> Result<(), GuestError> {
    proto::write_frame(stream, &proto::Frame::json(kind, value))
        .map_err(io_err("write to vibe-guest"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    use vibe_guest::proto::{FrameKind, TermHello};

    /// A stand-in for `vibe-vm` with a guest behind it: each connection's
    /// `connect` is accepted, and the next canned answer is what the guest
    /// sends back. What the guest was sent is kept.
    struct FakeVm {
        socket: PathBuf,
        _dir: tempfile::TempDir,
        _lock: File,
        sent: Arc<Mutex<Vec<(u32, String)>>>,
    }

    enum Answer {
        Line(&'static str),
        Frames(Vec<proto::Frame>),
    }

    impl FakeVm {
        fn serve(answers: Vec<Answer>) -> FakeVm {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("x.vm");
            let lock = File::create(dir.path().join("x.vm.lock")).unwrap();
            lock.lock().unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            let sent = Arc::new(Mutex::new(Vec::new()));
            let seen = sent.clone();
            std::thread::spawn(move || {
                for (stream, answer) in listener.incoming().zip(answers) {
                    let mut stream = stream.unwrap();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let request: vm::Request = serde_json::from_str(&line).unwrap();
                    let vm::Request::Connect { port } = request else {
                        panic!("not a connect: {line}")
                    };
                    stream.write_all(b"{\"ok\":true}\n").unwrap();
                    match answer {
                        Answer::Line(reply) => {
                            let mut request = String::new();
                            reader.read_line(&mut request).unwrap();
                            seen.lock()
                                .unwrap()
                                .push((port, request.trim_end().to_owned()));
                            stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
                        }
                        Answer::Frames(frames) => {
                            let hello = proto::read_frame(&mut reader).unwrap().unwrap();
                            let hello: TermHello = hello.decode().unwrap();
                            seen.lock()
                                .unwrap()
                                .push((port, serde_json::to_string(&hello).unwrap()));
                            for f in frames {
                                proto::write_frame(&mut stream, &f).unwrap();
                            }
                        }
                    }
                }
            });
            FakeVm {
                socket,
                _dir: dir,
                _lock: lock,
                sent,
            }
        }

        fn sent(&self) -> Vec<(u32, String)> {
            self.sent.lock().unwrap().clone()
        }
    }

    fn session() -> Session {
        Session {
            pid: 42,
            command: vec!["claude".into()],
            cwd: "/w".into(),
            started_ms: 900,
            exit: None,
        }
    }

    #[test]
    fn calls_reach_the_control_port_and_read_replies() {
        let vm = FakeVm::serve(vec![
            Answer::Line(r#"{"ok":true,"pid":42}"#),
            Answer::Line(
                r#"{"ok":true,"processes":3,"freeze_ms":4,"sync_ms":5,"memory":{"total":4096,"available":1024}}"#,
            ),
            Answer::Line(r#"{"ok":true,"clock_step_ms":30000,"shares_ms":900,"thaw_ms":2}"#),
            Answer::Line(
                r#"{"ok":false,"code":"busy","error":"a command is already running, as pid 42"}"#,
            ),
            Answer::Line(r#"{"ok":true,"pid":"not a number"}"#),
            Answer::Line(
                r#"{"ok":true,"protocol":9,"version":"x","boot_id":"b","uptime_ms":1,"state":"idle","frozen":false,"memory":{"total":1,"available":1}}"#,
            ),
        ]);
        let mut l = Launch {
            command: vec!["claude".into()],
            env: BTreeMap::from([("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-secret".into())]),
            cwd: Some("/w".into()),
            size: None,
            time_ms: Some(5),
        };
        assert_eq!(launch(&vm.socket, &l).unwrap().pid, 42);
        let frozen = freeze(&vm.socket).unwrap();
        assert_eq!(frozen.memory.available, 1024);
        let at = UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        let thawed = thaw(&vm.socket, at, Some(Duration::from_secs(3))).unwrap();
        assert_eq!(thawed.clock_step_ms, Some(30000));
        let err = launch(&vm.socket, &l).unwrap_err();
        assert_eq!(err.code(), Some(ErrorCode::Busy));
        assert!(matches!(
            launch(&vm.socket, &l),
            Err(GuestError::Protocol(_))
        ));
        assert!(matches!(status(&vm.socket), Err(GuestError::Protocol(_))));
        let sent = vm.sent();
        assert!(sent.iter().all(|(port, _)| *port == proto::CONTROL_PORT));
        assert_eq!(
            sent[0].1,
            r#"{"op":"launch","command":["claude"],"env":{"CLAUDE_CODE_OAUTH_TOKEN":"sk-secret"},"cwd":"/w","time_ms":5}"#
        );
        assert_eq!(sent[1].1, r#"{"op":"freeze"}"#);
        assert_eq!(
            sent[2].1,
            r#"{"op":"thaw","time_ms":1700000000123,"share_timeout_ms":3000}"#
        );
        // A launch the guest would refuse never leaves the host.
        l.command.clear();
        assert_eq!(
            launch(&vm.socket, &l).unwrap_err().code(),
            Some(ErrorCode::BadRequest)
        );
        assert_eq!(vm.sent().len(), 6);
    }

    #[test]
    fn attach_says_hello_and_hands_over_the_stream() {
        let welcome = TermWelcome {
            protocol: proto::PROTOCOL,
            session: session(),
            size: TermSize {
                rows: 40,
                cols: 120,
            },
        };
        let vm = FakeVm::serve(vec![
            Answer::Frames(vec![
                proto::Frame::json(FrameKind::Welcome, &welcome),
                proto::Frame::new(FrameKind::Output, b"screen".to_vec()),
            ]),
            Answer::Frames(vec![proto::Frame::json(
                FrameKind::Refused,
                &proto::Refused {
                    message: "nothing is running in the guest yet".into(),
                    protocol: proto::PROTOCOL,
                },
            )]),
        ]);
        let mut t = attach(
            &vm.socket,
            "vibe-shim",
            Some(TermSize {
                rows: 40,
                cols: 120,
            }),
        )
        .unwrap();
        assert_eq!(t.welcome, welcome);
        let f = proto::read_frame(&mut t.stream).unwrap().unwrap();
        assert_eq!(f.payload, b"screen");
        let err = attach(&vm.socket, "vibe-shim", None).unwrap_err();
        assert!(err.to_string().contains("nothing is running"), "{err}");
        let sent = vm.sent();
        assert!(sent.iter().all(|(port, _)| *port == proto::TERMINAL_PORT));
        assert_eq!(
            sent[0].1,
            r#"{"protocol":1,"client":"vibe-shim","size":{"rows":40,"cols":120}}"#
        );
    }

    #[test]
    fn the_balloon_leaves_headroom() {
        const GIB: u64 = 1 << 30;
        let m = |total, available| Memory { total, available };
        // 1 GiB used: a quarter of it more.
        assert_eq!(balloon_floor(m(4 * GIB, 3 * GIB)), GIB + GIB / 4);
        // Little used: at least 256 MiB more.
        assert_eq!(
            balloon_floor(m(4 * GIB, 4 * GIB - (100 << 20))),
            (100 << 20) + MIN_HEADROOM
        );
        // Never more than there is.
        assert_eq!(balloon_floor(m(GIB, 0)), GIB);
        assert_eq!(balloon_floor(Memory::default()), 0);
    }

    #[test]
    fn nothing_listening_is_a_vm_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = status(&dir.path().join("none.vm")).unwrap_err();
        assert!(matches!(err, GuestError::Vm(_)), "{err}");
        let err = wait_ready(&dir.path().join("none.vm"), Duration::from_millis(100)).unwrap_err();
        assert!(matches!(err, GuestError::Vm(_)), "{err}");
    }
}
