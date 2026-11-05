//! The protocol of a `vibe-shim` socket, `run/<ref>.sock` (PRD:
//! Architecture). Only Rust speaks it: `vibe attach`, which streams a
//! terminal, and `vibed`, which asks the shim what it holds. The Swift app
//! never connects to a shim; it opens Terminal running `vibe attach`.
//!
//! Every message is a frame: a kind byte, a big-endian `u32` length, then
//! that many bytes of payload. Terminal bytes travel as they are; everything
//! else is JSON. A client opens with [`FrameKind::Hello`] and the shim
//! answers [`FrameKind::Welcome`], or [`FrameKind::Refused`] and closes.
//!
//! - An **attach** client then gets the screen redrawn as one
//!   [`FrameKind::Output`], and from then on everything the agent writes. It
//!   sends keystrokes as [`FrameKind::Input`] and its size as
//!   [`FrameKind::Resize`]. To detach it just closes the connection.
//! - A **control** client sends [`Request`]s and gets a [`Reply`] to each,
//!   in order. It may also send input, as `vibe_send` will.
//!
//! When the command exits, every attach client gets a [`FrameKind::Exit`]
//! and the shim closes. It leaves a [`Record`] in its state directory.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::rpc::SessionExit;

/// Bumped on any incompatible change. Both sides send theirs; the shim
/// refuses a client that speaks another.
pub const PROTOCOL: u32 = 1;

/// The largest payload either side sends or accepts. Terminal bytes go in
/// far smaller frames; this bounds a `read_output` reply.
pub const MAX_FRAME: usize = 16 << 20;

/// The kind byte and the length.
pub const HEADER_LEN: usize = 5;

/// The record a shim leaves in its state directory when its command exits.
pub const RECORD_FILE: &str = "session.json";

/// The shim's own log, in its state directory.
pub const LOG_FILE: &str = "shim.log";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// Client → shim, first: a [`Hello`].
    Hello = 1,
    /// Shim → client: a [`Welcome`].
    Welcome = 2,
    /// Client → shim: bytes for the terminal, as typed.
    Input = 3,
    /// Shim → attach client: bytes the terminal wrote.
    Output = 4,
    /// Attach client → shim: its new [`Size`].
    Resize = 5,
    /// Control client → shim: a [`Request`].
    Request = 6,
    /// Shim → control client: the [`Reply`] to the oldest open request.
    Reply = 7,
    /// Shim → attach client: the command ended; a [`SessionExit`].
    Exit = 8,
    /// Shim → client, instead of a welcome: a [`Refused`]; then it closes.
    Refused = 9,
}

impl FrameKind {
    const ALL: [FrameKind; 9] = [
        FrameKind::Hello,
        FrameKind::Welcome,
        FrameKind::Input,
        FrameKind::Output,
        FrameKind::Resize,
        FrameKind::Request,
        FrameKind::Reply,
        FrameKind::Exit,
        FrameKind::Refused,
    ];

    pub fn from_byte(b: u8) -> Option<FrameKind> {
        Self::ALL.into_iter().find(|k| *k as u8 == b)
    }
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
            serde_json::to_vec(value).expect("shim messages serialize to JSON"),
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

    /// The frame as it goes on the wire. A payload over [`MAX_FRAME`] is
    /// the caller's bug; split terminal bytes before framing them.
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

/// A terminal's size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    /// What a session starts at before anyone attaches.
    pub const DEFAULT: Size = Size { rows: 24, cols: 80 };

    /// The most rows or columns a session takes. Every cell costs memory in
    /// the screen model, so a client can't ask for billions.
    pub const MAX_CELLS_PER_SIDE: u16 = 1000;

    /// Zero means "unknown" to the kernel, so a screen needs at least a
    /// cell, and at most [`Size::MAX_CELLS_PER_SIDE`] a side.
    pub fn is_valid(self) -> bool {
        (1..=Self::MAX_CELLS_PER_SIDE).contains(&self.rows)
            && (1..=Self::MAX_CELLS_PER_SIDE).contains(&self.cols)
    }
}

impl fmt::Display for Size {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.cols, self.rows)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// A terminal: gets the screen and everything written, and counts as
    /// attached.
    Attach,
    /// A program asking questions, such as `vibed`. It isn't counted as
    /// attached and gets no output stream.
    Control,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol: u32,
    pub role: Role,
    /// Who is connecting, for the shim's log, e.g. `vibe-cli` or `vibed`.
    pub client: String,
    /// An attach client's terminal device, such as `/dev/ttys003`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    /// An attach client's terminal size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    #[serde(rename = "ref")]
    pub agent: String,
    /// The shim's pid, which is also the session ID of every process the
    /// agent starts.
    pub pid: u32,
    /// The command's pid. For a `linux` agent's shim (`vibe-shim --vm`),
    /// that's its pid in the guest, which means nothing on this host.
    pub child_pid: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Refused {
    pub message: String,
    /// The shim's [`PROTOCOL`].
    pub protocol: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    /// What the shim holds: a [`Status`].
    Status,
    /// The screen, and the lines that scrolled off it since the cursor
    /// `after` (all that are kept, if absent): an [`Output`].
    ReadOutput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Status(Status),
    Output(Output),
    Error(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    #[serde(rename = "ref")]
    pub agent: String,
    pub pid: u32,
    /// As in [`Welcome::child_pid`]: in the guest, for a `linux` agent.
    pub child_pid: u32,
    pub command: Vec<String>,
    pub cwd: String,
    pub started_at: String,
    /// The terminal's size now: the size of the client that was active
    /// last.
    pub size: Size,
    /// The attach clients, in the order they attached.
    pub clients: Vec<Client>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub client: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<Size>,
    pub attached_at: String,
}

/// What the terminal shows, as plain text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    /// The screen, one line per row, trailing blanks trimmed.
    pub screen: String,
    /// Lines that scrolled off the top of the screen since the cursor asked
    /// for, oldest first.
    pub history: Vec<String>,
    /// The line after the last one in `history`; pass it as `after` next
    /// time to get only what's new. When `more` is false, that's how many
    /// lines have scrolled off in all.
    pub cursor: u64,
    /// Lines after the cursor asked for that are no longer kept.
    pub dropped: u64,
    /// The history didn't all fit in one reply; ask again from `cursor` for
    /// the rest.
    #[serde(default)]
    pub more: bool,
    /// The program has turned on bracketed paste (`CSI ? 2004 h`), so a
    /// paste should be wrapped in its markers, as a terminal would.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bracketed_paste: bool,
}

/// What a shim leaves in its state directory when its command exits, so the
/// end of a session can be read after the shim is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(rename = "ref")]
    pub agent: String,
    pub command: Vec<String>,
    pub cwd: String,
    pub started_at: String,
    pub exit: SessionExit,
    /// The last screen and the history kept.
    pub output: Output,
    /// How wide the terminal was at the end, which says where the screen's lines
    /// were wrapped. None in a record from a shim before this was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
}

/// The name of a signal, as in `SIGKILL`, for the common ones.
pub fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "SIGHUP",
        libc::SIGINT => "SIGINT",
        libc::SIGQUIT => "SIGQUIT",
        libc::SIGILL => "SIGILL",
        libc::SIGTRAP => "SIGTRAP",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGFPE => "SIGFPE",
        libc::SIGKILL => "SIGKILL",
        libc::SIGUSR1 => "SIGUSR1",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGUSR2 => "SIGUSR2",
        libc::SIGPIPE => "SIGPIPE",
        libc::SIGALRM => "SIGALRM",
        libc::SIGTERM => "SIGTERM",
        libc::SIGXCPU => "SIGXCPU",
        libc::SIGXFSZ => "SIGXFSZ",
        _ => return format!("signal {signal}"),
    };
    name.to_owned()
}

/// Why a connection to a shim failed.
#[derive(Debug)]
pub enum ConnectError {
    /// Nothing is serving the socket: the shim is gone.
    Gone(io::Error),
    Io(io::Error),
    /// The shim turned the client away.
    Refused(Refused),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Gone(e) => write!(f, "the session isn't running ({e})"),
            ConnectError::Io(e) => write!(f, "talking to the session: {e}"),
            ConnectError::Refused(r) => write!(f, "the session refused: {}", r.message),
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<io::Error> for ConnectError {
    fn from(e: io::Error) -> Self {
        ConnectError::Io(e)
    }
}

/// A blocking connection to a shim, past the handshake.
#[derive(Debug)]
pub struct Connection {
    pub stream: UnixStream,
    pub welcome: Welcome,
}

/// Connects to the shim at `socket` and says `hello`. Each read and write
/// waits at most `timeout`; [`Connection::stream`] keeps that timeout.
pub fn connect(
    socket: &Path,
    hello: &Hello,
    timeout: Duration,
) -> Result<Connection, ConnectError> {
    let mut stream = UnixStream::connect(socket).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => ConnectError::Gone(e),
        _ => ConnectError::Io(e),
    })?;
    // Never zero, which a socket refuses.
    let timeout = timeout.max(Duration::from_millis(1));
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write_frame(&mut stream, &Frame::json(FrameKind::Hello, hello))?;
    let frame = read_frame(&mut stream)?.ok_or_else(|| {
        ConnectError::Gone(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "the shim closed the connection",
        ))
    })?;
    match frame.kind {
        FrameKind::Welcome => Ok(Connection {
            welcome: frame.decode()?,
            stream,
        }),
        FrameKind::Refused => Err(ConnectError::Refused(frame.decode()?)),
        other => Err(ConnectError::Io(invalid(format!(
            "a {other:?} frame instead of a welcome"
        )))),
    }
}

impl Connection {
    /// Sends `request` on a control connection and waits for its reply.
    pub fn request(&mut self, request: &Request) -> io::Result<Reply> {
        write_frame(&mut self.stream, &Frame::json(FrameKind::Request, request))?;
        loop {
            let frame = read_frame(&mut self.stream)?
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "the shim closed"))?;
            match frame.kind {
                FrameKind::Reply => return frame.decode(),
                // A control client gets nothing else, but it's harmless.
                FrameKind::Output | FrameKind::Exit => continue,
                other => return Err(invalid(format!("a {other:?} frame instead of a reply"))),
            }
        }
    }

    /// [`Request::Status`].
    pub fn status(&mut self) -> io::Result<Status> {
        match self.request(&Request::Status)? {
            Reply::Status(s) => Ok(s),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(invalid(format!("{other:?} instead of a status"))),
        }
    }

    /// [`Request::ReadOutput`].
    pub fn read_output(&mut self, after: Option<u64>) -> io::Result<Output> {
        match self.request(&Request::ReadOutput { after })? {
            Reply::Output(o) => Ok(o),
            Reply::Error(e) => Err(io::Error::other(e)),
            other => Err(invalid(format!("{other:?} instead of output"))),
        }
    }
}

/// A control [`Hello`] from `client`.
pub fn control_hello(client: &str) -> Hello {
    Hello {
        protocol: PROTOCOL,
        role: Role::Control,
        client: client.to_owned(),
        tty: None,
        size: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn frames_round_trip() {
        let frames = [
            Frame::new(FrameKind::Input, b"ls\r".to_vec()),
            Frame::new(FrameKind::Output, Vec::new()),
            Frame::json(
                FrameKind::Resize,
                &Size {
                    rows: 50,
                    cols: 120,
                },
            ),
        ];
        let mut wire = Vec::new();
        for f in &frames {
            write_frame(&mut wire, f).unwrap();
        }
        assert_eq!(&wire[..HEADER_LEN], &[3, 0, 0, 0, 3]);
        let mut r = &wire[..];
        for f in &frames {
            assert_eq!(read_frame(&mut r).unwrap().as_ref(), Some(f));
        }
        assert_eq!(read_frame(&mut r).unwrap(), None, "a clean end");
        assert_eq!(
            frames[2].decode::<Size>().unwrap(),
            Size {
                rows: 50,
                cols: 120
            }
        );
    }

    #[test]
    fn bad_frames_are_errors() {
        // An unknown kind.
        assert!(read_frame(&mut &[0u8, 0, 0, 0, 0][..]).is_err());
        assert!(read_frame(&mut &[42u8, 0, 0, 0, 0][..]).is_err());
        // Too large, refused before any of it is read.
        let len = (MAX_FRAME as u32 + 1).to_be_bytes();
        let header = [4, len[0], len[1], len[2], len[3]];
        assert!(read_frame(&mut &header[..]).is_err());
        // Cut off in the header, or in the payload.
        assert!(read_frame(&mut &[4u8, 0][..]).is_err());
        assert!(read_frame(&mut &[4u8, 0, 0, 0, 2, b'x'][..]).is_err());
        // JSON that isn't the type.
        let f = Frame::new(FrameKind::Resize, b"{}".to_vec());
        assert!(f.decode::<Size>().is_err());
    }

    #[test]
    fn output_is_split_into_frames_that_fit() {
        let big = vec![b'x'; MAX_FRAME * 2 + 1];
        let lens: Vec<usize> = output_frames(FrameKind::Output, &big)
            .map(|f| f.payload.len())
            .collect();
        assert_eq!(lens, [MAX_FRAME, MAX_FRAME, 1]);
        assert_eq!(output_frames(FrameKind::Output, b"").count(), 0);
    }

    #[test]
    fn messages_on_the_wire() {
        assert_eq!(
            serde_json::to_value(Request::ReadOutput { after: Some(3) }).unwrap(),
            json!({"method": "read_output", "after": 3})
        );
        assert_eq!(
            serde_json::to_value(Request::Status).unwrap(),
            json!({"method": "status"})
        );
        assert_eq!(
            serde_json::from_value::<Request>(json!({"method": "read_output"})).unwrap(),
            Request::ReadOutput { after: None }
        );
        assert!(serde_json::from_value::<Request>(json!({"method": "kill"})).is_err());
        assert_eq!(
            serde_json::to_value(Reply::Error("no".into())).unwrap(),
            json!({"error": "no"})
        );
        assert_eq!(
            serde_json::to_value(control_hello("vibed")).unwrap(),
            json!({"protocol": PROTOCOL, "role": "control", "client": "vibed"})
        );
        let attach: Hello = serde_json::from_value(json!({
            "protocol": 1, "role": "attach", "client": "vibe-cli",
            "tty": "/dev/pts/3", "size": {"rows": 40, "cols": 100}
        }))
        .unwrap();
        assert_eq!(
            attach.size,
            Some(Size {
                rows: 40,
                cols: 100
            })
        );
        assert!(
            serde_json::from_value::<Hello>(json!({"protocol": 1, "role": "spy", "client": "x"}))
                .is_err()
        );
    }

    #[test]
    fn sizes() {
        assert!(Size::DEFAULT.is_valid());
        assert!(!Size { rows: 0, cols: 80 }.is_valid());
        assert!(
            Size {
                rows: 1000,
                cols: 1000
            }
            .is_valid()
        );
        assert!(
            !Size {
                rows: 65535,
                cols: 65535
            }
            .is_valid()
        );
        assert!(
            !Size {
                rows: 24,
                cols: 1001
            }
            .is_valid()
        );
        assert_eq!(Size { rows: 24, cols: 80 }.to_string(), "80x24");
    }

    #[test]
    fn signal_names() {
        assert_eq!(signal_name(libc::SIGKILL), "SIGKILL");
        assert_eq!(signal_name(libc::SIGHUP), "SIGHUP");
        assert_eq!(signal_name(99), "signal 99");
    }

    #[test]
    fn connecting_to_nothing_says_the_session_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let err = connect(
            &dir.path().join("x.sock"),
            &control_hello("t"),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(err, ConnectError::Gone(_)), "{err}");
        // A socket file nobody serves.
        let path = dir.path().join("stale.sock");
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        let err = connect(&path, &control_hello("t"), Duration::from_secs(1)).unwrap_err();
        assert!(matches!(err, ConnectError::Gone(_)), "{err}");
    }

    #[test]
    fn a_refusal_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let hello: Hello = read_frame(&mut s).unwrap().unwrap().decode().unwrap();
            assert_eq!(hello.client, "t");
            let refused = Refused {
                message: "speak 1".into(),
                protocol: 1,
            };
            write_frame(&mut s, &Frame::json(FrameKind::Refused, &refused)).unwrap();
        });
        let err = connect(&path, &control_hello("t"), Duration::from_secs(5)).unwrap_err();
        assert!(
            matches!(&err, ConnectError::Refused(r) if r.message == "speak 1"),
            "{err}"
        );
    }
}
