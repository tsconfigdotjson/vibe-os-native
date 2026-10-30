//! What `vibe-guest` serves, on two vsock ports in the guest. The host
//! reaches them only through `vibe-vm`'s `connect`, since only the process
//! that owns a VM can reach its vsock (`protocol/vibe-vm/`).
//!
//! - [`CONTROL_PORT`]: `vibed`'s channel. A connection carries one
//!   [`Request`] line and gets one reply line, `{"ok":true, …}` or
//!   `{"ok":false,"code":…,"error":…}` ([`encode_reply`]), as on `vibe-vm`'s
//!   own socket.
//! - [`TERMINAL_PORT`]: the agent's terminal, for `vibe-shim`. Frames as on a
//!   shim's socket (a kind byte, a big-endian `u32` length, the payload):
//!   the shim says [`TermHello`], the guest answers [`TermWelcome`] and a
//!   redraw of the screen, then output flows one way and input and resizes
//!   the other. When the command exits the shim gets an [`Exit`]. One shim
//!   at a time: a new connection takes over from the last, which is
//!   closed. If the connection drops, the command keeps running, and the
//!   next shim to connect gets the screen as it is by then.
//!
//! The contract, with an example of every request and reply, is in
//! `protocol/vibe-guest/`.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Bumped on any incompatible change. The status reply carries it, and both
/// ends of the terminal send theirs.
pub const PROTOCOL: u32 = 1;

/// The vsock port of the control channel.
pub const CONTROL_PORT: u32 = 1100;

/// The vsock port of the agent's terminal.
pub const TERMINAL_PORT: u32 = 1101;

/// The longest request or reply line. A launch carries the command, its
/// environment and the Claude token, far less than this.
pub const MAX_LINE: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// The control channel

/// A request on the control port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// What the guest is and holds: a [`Status`].
    Status,
    /// Starts the agent's command on the terminal, as `vibe`: a
    /// [`Launched`]. Refused with `busy` while one runs; after one exits,
    /// a launch starts the next (a `claude --resume`, say).
    Launch(Launch),
    /// Every process the command has started, and its own: [`Processes`],
    /// for the idle rule (PRD: Going to sleep).
    Processes,
    /// Freezes the command and everything it started, then flushes every
    /// disk and share: [`Frozen`]. Before a save.
    Freeze,
    /// After a restore: sets the clock to `time_ms`, waits for the shares to
    /// answer again, then thaws the command: [`Thawed`]. It always thaws,
    /// and says what didn't go to plan.
    Thaw {
        /// The host's clock, in milliseconds since the Unix epoch.
        time_ms: u64,
        /// How long to wait for the shares, at most; absent for
        /// [`SHARE_TIMEOUT_MS`].
        #[serde(default, skip_serializing_if = "Option::is_none")]
        share_timeout_ms: Option<u64>,
    },
    /// Sets the clock: a [`ClockSet`]. A VM has no clock of its own to
    /// boot with.
    Clock { time_ms: u64 },
    /// Ends the command and everything it started, flushes the disks and
    /// powers the VM off, after replying [`Done`]. `vibe-vm` then exits 0.
    Poweroff,
}

/// How long a thaw waits for the shares by default. They answer about
/// 0.9 s after a restore (PRD: Spike results).
pub const SHARE_TIMEOUT_MS: u64 = 10_000;

/// What to run, and how. The environment carries the Claude token, so
/// [`fmt::Debug`] shows only its names, and the guest never writes it down.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    /// The program and its arguments. A bare name is looked up on the
    /// guest's `PATH`.
    pub command: Vec<String>,
    /// Added to the guest's own environment for the command (`HOME`,
    /// `PATH`, `XDG_RUNTIME_DIR` and the rest), and taking precedence.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Where it starts; absent for `vibe`'s home.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// The terminal's size until a shim says otherwise; absent for
    /// [`TermSize::DEFAULT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TermSize>,
    /// If set, the clock is set to this first, as with [`Request::Clock`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time_ms: Option<u64>,
}

impl fmt::Debug for Launch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Launch")
            .field("command", &self.command)
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("size", &self.size)
            .field("time_ms", &self.time_ms)
            .finish()
    }
}

impl Launch {
    /// Why the guest would refuse it, if it would.
    pub fn validate(&self) -> Result<(), String> {
        if self.command.is_empty() || self.command[0].is_empty() {
            return Err("the command is empty".into());
        }
        if self.command.iter().any(|a| a.contains('\0')) {
            return Err("the command has a NUL byte".into());
        }
        for (key, value) in &self.env {
            if key.is_empty() || key.contains(['=', '\0']) {
                return Err(format!("`{key}` isn't an environment variable name"));
            }
            if value.contains('\0') {
                return Err(format!("the value of {key} has a NUL byte"));
            }
        }
        if let Some(cwd) = &self.cwd
            && (!cwd.starts_with('/') || cwd.contains('\0'))
        {
            return Err(format!("cwd `{cwd}` must be an absolute path"));
        }
        if let Some(size) = self.size
            && !size.is_valid()
        {
            return Err(format!("{size} isn't a terminal size"));
        }
        Ok(())
    }
}

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub rows: u16,
    pub cols: u16,
}

impl TermSize {
    pub const DEFAULT: TermSize = TermSize { rows: 24, cols: 80 };

    /// At least a cell and at most 1000 a side, as a shim takes.
    pub fn is_valid(self) -> bool {
        (1..=1000).contains(&self.rows) && (1..=1000).contains(&self.cols)
    }
}

impl fmt::Display for TermSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

/// Whether the guest has run a command yet, and whether it still runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GuestState {
    /// Nothing launched since boot.
    Idle,
    Running,
    /// The command exited; a launch starts another.
    Exited,
}

/// The command the guest is running, or ran last.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    /// Its pid in the guest.
    pub pid: u32,
    pub command: Vec<String>,
    pub cwd: String,
    /// The guest's uptime when it started. Uptime stops while the VM
    /// sleeps, like the rest of the guest.
    pub started_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<Exit>,
}

/// How the command ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<i32>,
    /// The signal that ended it, by name, such as `SIGKILL`: numbers differ
    /// between Linux and macOS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
}

/// The guest's memory, in bytes, from `/proc/meminfo`. What it really uses
/// is `total - available`; `vibed` sets the balloon's floor from that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Memory {
    pub total: u64,
    pub available: u64,
}

/// The address DHCP gave the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Network {
    pub interface: String,
    /// With its prefix length, as in `192.168.64.5/24`.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
}

/// A share the guest mounted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareMount {
    pub tag: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub protocol: u32,
    /// `vibe-guest`'s version.
    pub version: String,
    /// The kernel's boot id: it changes on every boot, never on a restore.
    pub boot_id: String,
    pub uptime_ms: u64,
    pub state: GuestState,
    /// Whether the command is frozen.
    pub frozen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<Session>,
    pub memory: Memory,
    /// None until DHCP answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Network>,
    /// The disks stacked into the root, top first; empty if the guest runs
    /// from its initrd.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub layers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shares: Vec<ShareMount>,
    /// What went wrong since boot that the guest got past, such as a share
    /// that wouldn't mount.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launched {
    /// The command's pid in the guest.
    pub pid: u32,
}

/// A process the command started, or the command itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// The kernel's name for it (`comm`), at most 15 bytes.
    pub name: String,
    /// Its command line, if it has one (a kernel thread or a zombie has
    /// none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Its state letter from `/proc`, such as `S` or `R`.
    pub state: String,
    /// The guest's uptime when it started.
    pub started_ms: u64,
    /// CPU time used so far, user and system.
    pub cpu_ms: u64,
    /// Resident memory, in bytes.
    pub rss: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Processes {
    /// The guest's uptime now, to set `started_ms` against.
    pub uptime_ms: u64,
    pub processes: Vec<Process>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frozen {
    /// How many processes were frozen.
    pub processes: u32,
    pub freeze_ms: u64,
    /// Flushing the disks and shares, so the host sees every write, the
    /// transcript's included, while the agent sleeps.
    pub sync_ms: u64,
    /// The memory as the freeze left it, for the balloon's floor.
    pub memory: Memory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thawed {
    /// How far the clock moved, forward if positive; absent if it couldn't
    /// be set (see `problems`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clock_step_ms: Option<i64>,
    /// How long the shares took to answer.
    pub shares_ms: u64,
    /// Shares that still didn't answer when the wait ran out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unanswered: Vec<String>,
    pub thaw_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockSet {
    /// How far the clock moved, forward if positive.
    pub step_ms: i64,
}

/// The reply to a poweroff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Done {}

/// Why the guest refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The request isn't one the guest knows, or is invalid.
    BadRequest,
    /// A command is already running.
    Busy,
    LaunchFailed,
    /// The command couldn't be frozen or thawed.
    FreezeFailed,
    ClockFailed,
}

impl ErrorCode {
    pub const ALL: [ErrorCode; 5] = [
        ErrorCode::BadRequest,
        ErrorCode::Busy,
        ErrorCode::LaunchFailed,
        ErrorCode::FreezeFailed,
        ErrorCode::ClockFailed,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub code: ErrorCode,
    pub error: String,
}

impl Failure {
    pub fn new(code: ErrorCode, error: impl Into<String>) -> Failure {
        Failure {
            code,
            error: error.into(),
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.error)
    }
}

/// One reply line, without its newline: `{"ok":true, …}` with `T`'s fields,
/// or `{"ok":false,"code":…,"error":…}`.
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

/// Reads a request line, saying why it isn't one.
pub fn decode_request(line: &str) -> Result<Request, Failure> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| Failure::new(ErrorCode::BadRequest, format!("not JSON: {e}")))?;
    if let Some(op) = value.get("op").and_then(Value::as_str)
        && !KNOWN_OPS.contains(&op)
    {
        return Err(Failure::new(
            ErrorCode::BadRequest,
            format!("unknown op `{op}`"),
        ));
    }
    let request: Request = serde_json::from_value(value)
        .map_err(|e| Failure::new(ErrorCode::BadRequest, e.to_string()))?;
    if let Request::Launch(launch) = &request {
        launch
            .validate()
            .map_err(|e| Failure::new(ErrorCode::BadRequest, e))?;
    }
    Ok(request)
}

const KNOWN_OPS: [&str; 7] = [
    "status",
    "launch",
    "processes",
    "freeze",
    "thaw",
    "clock",
    "poweroff",
];

// ---------------------------------------------------------------------------
// The terminal

/// The largest payload either end sends or accepts.
pub const MAX_FRAME: usize = 1 << 20;

/// The kind byte and the length.
pub const HEADER_LEN: usize = 5;

/// The numbers are those of the same frames on a shim's socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Shim → guest, first: a [`TermHello`].
    Hello = 1,
    /// Guest → shim: a [`TermWelcome`], then the screen as one
    /// [`FrameKind::Output`].
    Welcome = 2,
    /// Shim → guest: bytes for the terminal, as typed.
    Input = 3,
    /// Guest → shim: bytes the terminal wrote.
    Output = 4,
    /// Shim → guest: the terminal's new [`TermSize`].
    Resize = 5,
    /// Guest → shim: the command ended; an [`Exit`]. The guest then closes.
    Exit = 8,
    /// Guest → shim, instead of a welcome: a [`Refused`]; then it closes.
    Refused = 9,
}

impl FrameKind {
    const ALL: [FrameKind; 7] = [
        FrameKind::Hello,
        FrameKind::Welcome,
        FrameKind::Input,
        FrameKind::Output,
        FrameKind::Resize,
        FrameKind::Exit,
        FrameKind::Refused,
    ];

    pub fn from_byte(b: u8) -> Option<FrameKind> {
        Self::ALL.into_iter().find(|k| *k as u8 == b)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TermHello {
    pub protocol: u32,
    /// Who is connecting, for the guest's log, e.g. `vibe-shim`.
    pub client: String,
    /// The size the shim's terminal should be, applied before the screen
    /// is sent; absent to keep the size it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<TermSize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermWelcome {
    pub protocol: u32,
    pub session: Session,
    /// The terminal's size now.
    pub size: TermSize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub message: String,
    /// The guest's [`PROTOCOL`].
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(kind: FrameKind, payload: impl Into<Vec<u8>>) -> Frame {
        Frame {
            kind,
            payload: payload.into(),
        }
    }

    /// A frame whose payload is `value` as JSON.
    pub fn json(kind: FrameKind, value: &impl Serialize) -> Frame {
        Frame::new(
            kind,
            serde_json::to_vec(value).expect("terminal messages serialize to JSON"),
        )
    }

    /// The payload, read as JSON.
    pub fn decode<T: DeserializeOwned>(&self) -> io::Result<T> {
        serde_json::from_slice(&self.payload).map_err(|e| {
            invalid(format!(
                "a {:?} frame that isn't the expected JSON: {e}",
                self.kind
            ))
        })
    }

    /// The frame as it goes on the wire. A payload over [`MAX_FRAME`] is the
    /// caller's bug; [`output_frames`] splits terminal bytes.
    pub fn encode(&self) -> Vec<u8> {
        assert!(self.payload.len() <= MAX_FRAME, "frame too large");
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(self.kind as u8);
        out.extend_from_slice(&(self.payload.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// Reads a header: the kind and the payload length.
pub fn parse_header(header: [u8; HEADER_LEN]) -> io::Result<(FrameKind, usize)> {
    let kind = FrameKind::from_byte(header[0])
        .ok_or_else(|| invalid(format!("unknown frame kind {}", header[0])))?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(invalid(format!(
            "a frame of {len} bytes; the limit is {MAX_FRAME}"
        )));
    }
    Ok((kind, len))
}

/// Reads one frame, or `None` if the stream ends cleanly between frames.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    let (kind, len) = parse_header(header)?;
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame { kind, payload }))
}

pub fn write_frame(w: &mut impl Write, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode())
}

/// Terminal bytes as frames of at most [`MAX_FRAME`] bytes each.
pub fn output_frames(kind: FrameKind, bytes: &[u8]) -> impl Iterator<Item = Frame> + '_ {
    bytes.chunks(MAX_FRAME).map(move |c| Frame::new(kind, c))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch() -> Launch {
        Launch {
            command: vec!["claude".into(), "--session-id".into(), "x".into()],
            env: BTreeMap::from([("CLAUDE_CODE_OAUTH_TOKEN".into(), "sk-ant-secret".into())]),
            cwd: Some("/Users/me/VibeOS/worktrees/app/x".into()),
            size: Some(TermSize {
                rows: 40,
                cols: 120,
            }),
            time_ms: None,
        }
    }

    #[test]
    fn a_launch_never_shows_its_environment() {
        let shown = format!("{:?}", Request::Launch(launch()));
        assert!(shown.contains("CLAUDE_CODE_OAUTH_TOKEN"), "{shown}");
        assert!(!shown.contains("sk-ant-secret"), "{shown}");
    }

    #[test]
    fn launches_are_checked() {
        launch().validate().unwrap();
        type Change = fn(&mut Launch);
        let cases: [(Change, &str); 7] = [
            (|l| l.command.clear(), "empty"),
            (|l| l.command[0].clear(), "empty"),
            (|l| l.command.push("a\0b".into()), "NUL"),
            (|l| drop(l.env.insert("A=B".into(), "x".into())), "name"),
            (|l| drop(l.env.insert("A".into(), "\0".into())), "NUL"),
            (|l| l.cwd = Some("rel".into()), "absolute"),
            (|l| l.size = Some(TermSize { rows: 0, cols: 80 }), "size"),
        ];
        for (change, why) in cases {
            let mut l = launch();
            change(&mut l);
            let err = l.validate().unwrap_err();
            assert!(err.contains(why), "{err}");
            let line = serde_json::to_string(&Request::Launch(l)).unwrap();
            assert_eq!(
                decode_request(&line).unwrap_err().code,
                ErrorCode::BadRequest
            );
        }
    }

    #[test]
    fn requests_decode_or_say_why_not() {
        assert_eq!(decode_request(r#"{"op":"status"}"#), Ok(Request::Status));
        assert_eq!(
            decode_request(r#"{"op":"thaw","time_ms":5}"#),
            Ok(Request::Thaw {
                time_ms: 5,
                share_timeout_ms: None
            })
        );
        let err = decode_request(r#"{"op":"dance"}"#).unwrap_err();
        assert_eq!(err.error, "unknown op `dance`");
        for bad in ["", "[]", "{}", r#"{"op":"thaw"}"#, r#"{"op":"launch"}"#] {
            assert_eq!(
                decode_request(bad).unwrap_err().code,
                ErrorCode::BadRequest,
                "{bad}"
            );
        }
        // A launch is strict about its fields, so a typo isn't ignored.
        let err = decode_request(r#"{"op":"launch","command":["sh"],"enviroment":{}}"#);
        assert!(err.unwrap_err().error.contains("enviroment"));
    }

    #[test]
    fn replies_round_trip() {
        let ok: Result<Launched, Failure> = Ok(Launched { pid: 9 });
        assert_eq!(encode_reply(&ok), r#"{"ok":true,"pid":9}"#);
        let err: Result<Done, Failure> = Err(Failure::new(ErrorCode::Busy, "x"));
        let line = encode_reply(&err);
        assert_eq!(line, r#"{"code":"busy","error":"x","ok":false}"#);
        assert_eq!(decode_reply::<Done>(&line).unwrap(), err);
        for bad in ["", "[]", "{}", r#"{"ok":1}"#, r#"{"ok":true,"pid":"x"}"#] {
            assert!(decode_reply::<Launched>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frames_round_trip() {
        let mut wire = Vec::new();
        let hello = TermHello {
            protocol: PROTOCOL,
            client: "test".into(),
            size: Some(TermSize {
                rows: 50,
                cols: 160,
            }),
        };
        write_frame(&mut wire, &Frame::json(FrameKind::Hello, &hello)).unwrap();
        write_frame(&mut wire, &Frame::new(FrameKind::Input, b"ls\r".to_vec())).unwrap();
        let mut r = &wire[..];
        let f = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(f.decode::<TermHello>().unwrap(), hello);
        let f = read_frame(&mut r).unwrap().unwrap();
        assert_eq!((f.kind, &f.payload[..]), (FrameKind::Input, &b"ls\r"[..]));
        assert!(read_frame(&mut r).unwrap().is_none());
        // Half a header is an error, not a clean end.
        assert!(read_frame(&mut &wire[..3]).is_err());
        assert!(parse_header([7, 0, 0, 0, 0]).is_err());
        assert!(parse_header([4, 0xff, 0, 0, 0]).is_err());
        assert_eq!(
            output_frames(FrameKind::Output, &vec![0; MAX_FRAME + 1]).count(),
            2
        );
    }
}
