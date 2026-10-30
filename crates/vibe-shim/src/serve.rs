//! The running shim: one task owns the terminal's state, and every client
//! connection is a task that talks to it over a channel. Two threads move
//! bytes to and from the terminal, and for a PTY one waits for the command
//! to exit; a guest's terminal says so itself.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream as StdUnixStream;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinSet;
use vibe_core::rpc::SessionExit;
use vibe_core::shim::{
    self, Client, Frame, FrameKind, HEADER_LEN, Hello, PROTOCOL, Record, Refused, Reply, Request,
    Role, Size, Status, Welcome,
};
use vibe_core::time::{now, rfc3339};
use vibe_guest::proto as guest;

use crate::log;
use crate::pty::{self, Pty};
use crate::screen::Screen;

/// How long a new connection may take to say hello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// After the command exits, how long to wait for the rest of what it wrote.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// After the command exits, how long clients get to receive the end.
const GOODBYE_TIMEOUT: Duration = Duration::from_secs(2);

/// After hanging up on the command, how long before it's killed.
const HANGUP_GRACE: Duration = Duration::from_secs(5);

/// Output chunks kept for an attach client that's behind. One that falls
/// further behind is sent a fresh redraw instead.
const BACKLOG: usize = 256;

/// Input chunks waiting for a command that isn't reading, such as one that's
/// stopped. Past this, input is dropped rather than held.
const INPUT_BACKLOG: usize = 1024;

/// How long writing to a client may block before it counts as gone. A
/// terminal that stops reading mustn't stay attached forever.
const CLIENT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

/// The history a `read_output` reply carries at most, in bytes of text,
/// leaving room in the frame for JSON's escapes.
const REPLY_BUDGET: usize = shim::MAX_FRAME / 2;

/// What the session is: fixed at start.
#[derive(Debug)]
pub(crate) struct Session {
    pub(crate) agent: String,
    pub(crate) command: Vec<String>,
    pub(crate) cwd: String,
    pub(crate) state_dir: PathBuf,
    pub(crate) socket: PathBuf,
    pub(crate) started_at: i64,
    pub(crate) history: usize,
    pub(crate) size: Size,
}

/// What attach clients are sent.
#[derive(Debug, Clone)]
enum Out {
    Bytes(Arc<[u8]>),
    Exit(SessionExit),
}

/// What an attach client gets when it joins: everything up to now as a
/// redraw, then the stream from just after it.
#[derive(Debug)]
struct Joined {
    id: u64,
    redraw: Vec<u8>,
    rx: broadcast::Receiver<Out>,
}

#[derive(Debug)]
enum Event {
    Pty(Vec<u8>),
    PtyClosed,
    Exited(SessionExit),
    /// The guest's terminal dropped without saying the command ended: the
    /// VM went to sleep or stopped, or another shim took over.
    Gone(String),
    Attach {
        hello: Hello,
        reply: oneshot::Sender<Joined>,
    },
    Detach(u64),
    /// From an attach client (`Some(id)`) or a control client.
    Input(Option<u64>, Vec<u8>),
    Resize(u64, Size),
    Redraw(oneshot::Sender<(Vec<u8>, broadcast::Receiver<Out>)>),
    Request(Request, oneshot::Sender<Reply>),
}

#[derive(Debug)]
struct Attached {
    client: String,
    tty: Option<String>,
    size: Option<Size>,
    attached_at: i64,
    /// When it last attached, typed or resized, as a count; the highest
    /// sets the terminal's size.
    active: u64,
}

struct State {
    session: Session,
    welcome: Arc<Welcome>,
    screen: Screen,
    size: Size,
    end: End,
    input: std::sync::mpsc::SyncSender<ToTerm>,
    out: broadcast::Sender<Out>,
    clients: BTreeMap<u64, Attached>,
    next_id: u64,
    ticks: u64,
}

impl State {
    fn tick(&mut self) -> u64 {
        self.ticks += 1;
        self.ticks
    }

    /// Sizes the terminal for the client with a size that was active last.
    fn fit(&mut self) {
        let Some(size) = self
            .clients
            .values()
            .filter(|c| c.size.is_some())
            .max_by_key(|c| c.active)
            .and_then(|c| c.size)
        else {
            return;
        };
        if size == self.size {
            return;
        }
        match self.end {
            End::Pty(master) => {
                if let Err(e) = pty::set_size(master, size) {
                    log!("can't resize the terminal to {size}: {e}");
                    return;
                }
            }
            End::Guest => {
                if self.input.try_send(ToTerm::Resize(size)).is_err() {
                    log!("can't pass a resize to {size} on to the guest");
                    return;
                }
            }
        }
        self.size = size;
        self.screen.resize(size);
    }

    fn touch(&mut self, id: u64) {
        let tick = self.tick();
        if let Some(c) = self.clients.get_mut(&id) {
            c.active = tick;
        }
        self.fit();
    }

    fn status(&self) -> Status {
        Status {
            agent: self.session.agent.clone(),
            pid: self.welcome.pid,
            child_pid: self.welcome.child_pid,
            command: self.session.command.clone(),
            cwd: self.session.cwd.clone(),
            started_at: rfc3339(self.session.started_at),
            size: self.size,
            clients: self
                .clients
                .values()
                .map(|c| Client {
                    client: c.client.clone(),
                    tty: c.tty.clone(),
                    size: c.size,
                    attached_at: rfc3339(c.attached_at),
                })
                .collect(),
        }
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Pty(bytes) => {
                self.screen.process(&bytes);
                // No attach clients is fine.
                let _ = self.out.send(Out::Bytes(bytes.into()));
            }
            Event::PtyClosed | Event::Exited(_) | Event::Gone(_) => {}
            Event::Attach { hello, reply } => {
                let id = self.next_id;
                self.next_id += 1;
                let tick = self.tick();
                log!(
                    "{} attached on {} ({})",
                    hello.client,
                    hello.tty.as_deref().unwrap_or("no tty"),
                    hello.size.map_or("no size".into(), |s| s.to_string())
                );
                self.clients.insert(
                    id,
                    Attached {
                        client: hello.client,
                        tty: hello.tty,
                        size: hello.size.filter(|s| s.is_valid()),
                        attached_at: now(),
                        active: tick,
                    },
                );
                self.fit();
                let _ = reply.send(Joined {
                    id,
                    redraw: self.screen.redraw(),
                    rx: self.out.subscribe(),
                });
            }
            Event::Detach(id) => {
                if let Some(c) = self.clients.remove(&id) {
                    log!(
                        "{} on {} detached",
                        c.client,
                        c.tty.as_deref().unwrap_or("no tty")
                    );
                }
                self.fit();
            }
            Event::Input(id, bytes) => {
                if let Some(id) = id {
                    self.touch(id);
                }
                if let Err(std::sync::mpsc::TrySendError::Full(_)) =
                    self.input.try_send(ToTerm::Input(bytes))
                {
                    log!("the command isn't reading its input; dropped some");
                }
            }
            Event::Resize(id, size) => {
                if size.is_valid()
                    && let Some(c) = self.clients.get_mut(&id)
                {
                    c.size = Some(size);
                    self.touch(id);
                }
            }
            Event::Redraw(reply) => {
                let _ = reply.send((self.screen.redraw(), self.out.subscribe()));
            }
            Event::Request(request, reply) => {
                let answer = match request {
                    Request::Status => Reply::Status(self.status()),
                    Request::ReadOutput { after } => {
                        Reply::Output(self.screen.output(after, REPLY_BUDGET))
                    }
                };
                let _ = reply.send(answer);
            }
        }
    }

    /// Leaves the record of how the session ended, written whole or not at
    /// all.
    fn write_record(&self, exit: &SessionExit) -> io::Result<()> {
        let record = Record {
            agent: self.session.agent.clone(),
            command: self.session.command.clone(),
            cwd: self.session.cwd.clone(),
            started_at: rfc3339(self.session.started_at),
            exit: exit.clone(),
            output: self.screen.output(None, usize::MAX),
        };
        let path = self.session.state_dir.join(shim::RECORD_FILE);
        let tmp = path.with_extension("json.tmp");
        let mut json = serde_json::to_vec_pretty(&record).map_err(io::Error::other)?;
        json.push(b'\n');
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }
}

/// How the command ended, from its wait status.
fn session_exit(status: std::process::ExitStatus) -> SessionExit {
    SessionExit {
        code: status.code(),
        signal: status.signal().map(shim::signal_name),
        at: rfc3339(now()),
    }
}

/// Moves what the command writes into the loop, then says the terminal
/// closed.
fn read_pty(mut master: File, events: mpsc::Sender<Event>) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match master.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if events.blocking_send(Event::Pty(buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            // EIO: every slave descriptor is closed.
            Err(_) => break,
        }
    }
    let _ = events.blocking_send(Event::PtyClosed);
}

/// What goes to the terminal, in order.
#[derive(Debug)]
enum ToTerm {
    Input(Vec<u8>),
    /// Only for a guest's terminal; a PTY is resized at once.
    Resize(Size),
}

/// Which terminal the shim holds.
#[derive(Debug, Clone, Copy)]
enum End {
    /// A PTY's master end.
    Pty(RawFd),
    /// `vibe-guest`'s terminal, through `vibe-vm`.
    Guest,
}

/// The agent's terminal, as the shim got it.
#[derive(Debug)]
pub(crate) enum TerminalEnd {
    /// A command the shim started on a PTY (`macos`, a Linux host).
    Pty(Pty),
    /// The terminal of `vibe-guest` in a `linux` VM, past its welcome: the
    /// command runs there, and outlives the shim.
    Guest(StdUnixStream),
}

/// Writes input to the terminal, in order. A command that stops reading
/// blocks this thread only.
fn write_pty(mut master: File, input: std::sync::mpsc::Receiver<ToTerm>) {
    while let Ok(m) = input.recv() {
        if let ToTerm::Input(bytes) = m
            && master.write_all(&bytes).is_err()
        {
            return;
        }
    }
}

/// Moves what the guest's terminal sends into the loop: output, then the
/// exit if the command ends, or why the stream ended if it doesn't.
fn read_guest(mut stream: StdUnixStream, events: mpsc::Sender<Event>) {
    let why = loop {
        match guest::read_frame(&mut stream) {
            Ok(Some(f)) => match f.kind {
                guest::FrameKind::Output => {
                    if events.blocking_send(Event::Pty(f.payload)).is_err() {
                        return;
                    }
                }
                guest::FrameKind::Exit => {
                    let exit = match f.decode::<guest::Exit>() {
                        Ok(e) => SessionExit {
                            code: e.code,
                            signal: e.signal,
                            at: rfc3339(now()),
                        },
                        Err(e) => break e.to_string(),
                    };
                    let _ = events.blocking_send(Event::Exited(exit));
                    let _ = events.blocking_send(Event::PtyClosed);
                    return;
                }
                other => break format!("the guest sent a {other:?} frame"),
            },
            Ok(None) => break "the guest closed the terminal".into(),
            Err(e) => break format!("the guest's terminal failed: {e}"),
        }
    };
    let _ = events.blocking_send(Event::Gone(why));
}

/// Writes input and resizes to the guest's terminal, in order.
fn write_guest(mut stream: StdUnixStream, input: std::sync::mpsc::Receiver<ToTerm>) {
    while let Ok(m) = input.recv() {
        let sent = match m {
            ToTerm::Input(bytes) => guest::output_frames(guest::FrameKind::Input, &bytes)
                .try_for_each(|f| guest::write_frame(&mut stream, &f)),
            ToTerm::Resize(size) => {
                let size = guest::TermSize {
                    rows: size.rows,
                    cols: size.cols,
                };
                guest::write_frame(
                    &mut stream,
                    &guest::Frame::json(guest::FrameKind::Resize, &size),
                )
            }
        };
        if sent.is_err() {
            return;
        }
    }
}

/// Serves the session until the command exits, and returns how it ended;
/// or, for a guest's terminal, until the stream drops or the shim is asked
/// to stop, leaving the command running in the guest: `None`.
pub(crate) async fn run(
    session: Session,
    listener: std::os::unix::net::UnixListener,
    end: TerminalEnd,
    child_pid: u32,
) -> io::Result<Option<SessionExit>> {
    let (events_tx, mut events) = mpsc::channel::<Event>(256);
    let (input_tx, input_rx) = std::sync::mpsc::sync_channel(INPUT_BACKLOG);
    // Keep a PTY's master open for the resize ioctl until the end.
    let (end_kind, _master) = match end {
        TerminalEnd::Pty(Pty { master, mut child }) => {
            let master_fd = master.as_raw_fd();
            {
                let reader = master.try_clone()?;
                let tx = events_tx.clone();
                std::thread::Builder::new()
                    .name("pty-read".into())
                    .spawn(move || read_pty(reader, tx))?;
            }
            {
                let writer = master.try_clone()?;
                std::thread::Builder::new()
                    .name("pty-write".into())
                    .spawn(move || write_pty(writer, input_rx))?;
            }
            {
                let tx = events_tx.clone();
                std::thread::Builder::new()
                    .name("wait".into())
                    .spawn(move || {
                        let exit = match child.wait() {
                            Ok(status) => session_exit(status),
                            Err(e) => {
                                log!("can't wait for the command: {e}");
                                SessionExit {
                                    code: None,
                                    signal: None,
                                    at: rfc3339(now()),
                                }
                            }
                        };
                        let _ = tx.blocking_send(Event::Exited(exit));
                    })?;
            }
            (End::Pty(master_fd), Some(master))
        }
        TerminalEnd::Guest(stream) => {
            {
                let reader = stream.try_clone()?;
                let tx = events_tx.clone();
                std::thread::Builder::new()
                    .name("guest-read".into())
                    .spawn(move || read_guest(reader, tx))?;
            }
            std::thread::Builder::new()
                .name("guest-write".into())
                .spawn(move || write_guest(stream, input_rx))?;
            (End::Guest, None)
        }
    };

    let welcome = Arc::new(Welcome {
        protocol: PROTOCOL,
        agent: session.agent.clone(),
        pid: std::process::id(),
        child_pid,
    });
    let (out, _) = broadcast::channel(BACKLOG);
    let size = session.size;
    let screen = Screen::new(size, session.history);
    let mut state = State {
        session,
        welcome: welcome.clone(),
        screen,
        size,
        end: end_kind,
        input: input_tx,
        out,
        clients: BTreeMap::new(),
        next_id: 1,
        ticks: 0,
    };

    listener.set_nonblocking(true)?;
    let listener = UnixListener::from_std(listener)?;
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut tasks = JoinSet::new();
    let mut kill_at: Option<tokio::time::Instant> = None;
    let mut pty_closed = false;
    let guest = matches!(end_kind, End::Guest);

    let exit = loop {
        let kill_timer = async {
            match kill_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            event = events.recv() => match event {
                Some(Event::Exited(exit)) => break Some(exit),
                Some(Event::PtyClosed) => pty_closed = true,
                Some(Event::Gone(why)) => {
                    log!("{why}; the command goes on in the guest");
                    break None;
                }
                Some(event) => state.handle(event),
                None => unreachable!("the loop holds a sender"),
            },
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tasks.spawn(connection(stream, events_tx.clone(), welcome.clone()));
                }
                Err(e) => {
                    log!("accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
            // Being asked to stop hangs up on the command, as closing a
            // terminal window would; it then exits as usual. A guest's
            // command isn't the shim's to end: it goes on in the VM, and
            // `child_pid` is a pid there, not here.
            _ = term.recv() => if guest { break stop_leaving_guest() } else { hang_up(child_pid, &mut kill_at) },
            _ = int.recv() => if guest { break stop_leaving_guest() } else { hang_up(child_pid, &mut kill_at) },
            // We have no terminal of our own to lose; this is someone's kill.
            _ = hup.recv() => if guest { break stop_leaving_guest() } else { hang_up(child_pid, &mut kill_at) },
            () = kill_timer => {
                log!("the command ignored SIGHUP; killing it");
                kill_group(child_pid, libc::SIGKILL);
                kill_at = None;
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    };

    let Some(exit) = exit else {
        // No record: the session isn't over, and the next shim carries it
        // on. Clients see the connection close.
        let _ = std::fs::remove_file(&state.session.socket);
        drop(listener);
        drop(state);
        tasks.abort_all();
        return Ok(None);
    };
    describe(&exit);
    // The rest of what the command wrote, still serving the clients there
    // are; new ones wait in the listener's queue. The socket stays until
    // the record is written, so the session is never neither running nor
    // exited to someone asking.
    let drained = pty_closed
        || tokio::time::timeout(DRAIN_TIMEOUT, async {
            while let Some(event) = events.recv().await {
                match event {
                    Event::PtyClosed => return,
                    Event::Exited(_) | Event::Gone(_) => {}
                    event => state.handle(event),
                }
            }
        })
        .await
        .is_ok();
    if !drained {
        log!("the terminal is still open: something the command started holds it");
    }
    if let Err(e) = state.write_record(&exit) {
        log!(
            "can't write {}: {e}",
            state.session.state_dir.join(shim::RECORD_FILE).display()
        );
    }
    let _ = state.out.send(Out::Exit(exit.clone()));
    // Unlinked before it's closed: a client the close turns away then finds
    // no socket, so it knows the session is gone rather than broken.
    let _ = std::fs::remove_file(&state.session.socket);
    drop(listener);
    drop(state);
    // Clients waiting on the loop get nothing more; give them a moment to
    // send the end to their terminals.
    drop(events);
    drop(events_tx);
    let _ = tokio::time::timeout(GOODBYE_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    Ok(Some(exit))
}

fn stop_leaving_guest() -> Option<SessionExit> {
    log!("asked to stop; the command goes on in the guest");
    None
}

fn describe(exit: &SessionExit) {
    match (&exit.code, &exit.signal) {
        (Some(code), _) => log!("the command exited with code {code}"),
        (_, Some(signal)) => log!("the command was ended by {signal}"),
        _ => log!("the command ended"),
    }
}

/// Signals the command's process group.
fn kill_group(child_pid: u32, signal: libc::c_int) {
    // SAFETY: kill has no memory effects. The group is the command's own
    // (it made itself leader); a negative pid names the group.
    unsafe { libc::kill(-(child_pid as libc::pid_t), signal) };
}

fn hang_up(child_pid: u32, kill_at: &mut Option<tokio::time::Instant>) {
    log!("asked to stop; hanging up on the command");
    kill_group(child_pid, libc::SIGHUP);
    kill_at.get_or_insert_with(|| tokio::time::Instant::now() + HANGUP_GRACE);
}

/// Reads one frame; `None` at a clean end.
async fn read_frame(read: &mut OwnedReadHalf) -> io::Result<Option<Frame>> {
    let mut header = [0u8; HEADER_LEN];
    let mut got = 0;
    while got < HEADER_LEN {
        let n = read.read(&mut header[got..]).await?;
        if n == 0 {
            return if got == 0 {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        got += n;
    }
    let (kind, len) = shim::parse_header(header)?;
    let mut payload = vec![0; len];
    read.read_exact(&mut payload).await?;
    Ok(Some(Frame { kind, payload }))
}

/// Writes a frame, giving up on a client that stops reading for
/// [`CLIENT_WRITE_TIMEOUT`].
async fn write_frame(write: &mut OwnedWriteHalf, frame: &Frame) -> io::Result<()> {
    match tokio::time::timeout(CLIENT_WRITE_TIMEOUT, write.write_all(&frame.encode())).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the client stopped reading",
        )),
    }
}

async fn write_output(write: &mut OwnedWriteHalf, bytes: &[u8]) -> io::Result<()> {
    for frame in shim::output_frames(FrameKind::Output, bytes) {
        write_frame(write, &frame).await?;
    }
    Ok(())
}

/// Reads frames into a channel, so the connection's loop can wait on them
/// and on output at once without losing half a frame.
fn spawn_reader(mut read: OwnedReadHalf) -> mpsc::Receiver<io::Result<Frame>> {
    let (tx, rx) = mpsc::channel(16);
    tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut read).await {
                Ok(Some(f)) => Ok(f),
                Ok(None) => return,
                Err(e) => Err(e),
            };
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                return;
            }
        }
    });
    rx
}

async fn connection(stream: UnixStream, events: mpsc::Sender<Event>, welcome: Arc<Welcome>) {
    let (mut read, mut write) = stream.into_split();
    let hello = match tokio::time::timeout(HELLO_TIMEOUT, read_frame(&mut read)).await {
        Ok(Ok(Some(f))) if f.kind == FrameKind::Hello => {
            // The version first, so a newer client whose hello has fields
            // this one doesn't know still learns which version this speaks.
            let theirs = f
                .decode::<serde_json::Value>()
                .ok()
                .and_then(|v| v.get("protocol").and_then(serde_json::Value::as_u64));
            match theirs {
                Some(p) if p != u64::from(PROTOCOL) => {
                    return refuse(
                        &mut write,
                        format!(
                            "this shim speaks protocol {PROTOCOL}, not {p}; upgrade the client or restart the session"
                        ),
                    )
                    .await;
                }
                _ => f.decode::<Hello>(),
            }
        }
        Ok(Ok(Some(f))) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {:?} frame before hello", f.kind),
        )),
        Ok(Ok(None)) => return,
        Ok(Err(e)) => Err(e),
        Err(_) => Err(io::ErrorKind::TimedOut.into()),
    };
    let hello = match hello {
        Ok(h) if h.protocol == PROTOCOL => h,
        Ok(h) => {
            return refuse(
                &mut write,
                format!(
                    "this shim speaks protocol {PROTOCOL}, not {}; upgrade the client or restart the session",
                    h.protocol
                ),
            )
            .await;
        }
        Err(e) => return refuse(&mut write, format!("expected a hello: {e}")).await,
    };
    let frames = spawn_reader(read);
    let result = match hello.role {
        Role::Attach => attach(hello, frames, write, &events, &welcome).await,
        Role::Control => {
            if write_frame(&mut write, &Frame::json(FrameKind::Welcome, &*welcome))
                .await
                .is_err()
            {
                return;
            }
            control(frames, write, &events).await
        }
    };
    if let Err(e) = result
        && !matches!(
            e.kind(),
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
        )
    {
        log!("connection error: {e}");
    }
}

async fn refuse(write: &mut OwnedWriteHalf, message: String) {
    log!("refused a client: {message}");
    let refused = Refused {
        message,
        protocol: PROTOCOL,
    };
    let _ = write_frame(write, &Frame::json(FrameKind::Refused, &refused)).await;
}

/// An attach client: joined first, so it counts as attached and its size
/// applies by the time it's welcomed; then the redraw, then output both
/// ways until it leaves or the command exits.
async fn attach(
    hello: Hello,
    mut frames: mpsc::Receiver<io::Result<Frame>>,
    mut write: OwnedWriteHalf,
    events: &mpsc::Sender<Event>,
    welcome: &Welcome,
) -> io::Result<()> {
    let (reply, joined) = oneshot::channel();
    if events.send(Event::Attach { hello, reply }).await.is_err() {
        return Ok(());
    }
    let Ok(Joined { id, redraw, mut rx }) = joined.await else {
        return Ok(());
    };
    let result = async {
        write_frame(&mut write, &Frame::json(FrameKind::Welcome, welcome)).await?;
        write_output(&mut write, &redraw).await?;
        loop {
            tokio::select! {
                out = rx.recv() => match out {
                    Ok(Out::Bytes(bytes)) => write_output(&mut write, &bytes).await?,
                    Ok(Out::Exit(exit)) => {
                        write_frame(&mut write, &Frame::json(FrameKind::Exit, &exit)).await?;
                        let _ = write.shutdown().await;
                        return Ok(());
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Too far behind to catch up byte by byte: start
                        // over from the screen as it is now.
                        // If the session is ending, the loop is gone; what's
                        // left in the stream ends with the exit.
                        let (reply, redrawn) = oneshot::channel();
                        if events.send(Event::Redraw(reply)).await.is_err() {
                            continue;
                        }
                        let Ok((redraw, fresh)) = redrawn.await else { continue };
                        rx = fresh;
                        write_output(&mut write, &redraw).await?;
                    }
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
                frame = frames.recv() => match frame {
                    Some(Ok(f)) => match f.kind {
                        FrameKind::Input => {
                            let _ = events.send(Event::Input(Some(id), f.payload)).await;
                        }
                        FrameKind::Resize => {
                            let size = f.decode::<Size>()?;
                            let _ = events.send(Event::Resize(id, size)).await;
                        }
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!("an attach client sent a {other:?} frame"),
                            ));
                        }
                    },
                    Some(Err(e)) => return Err(e),
                    None => return Ok(()),
                },
            }
        }
    }
    .await;
    let _ = events.send(Event::Detach(id)).await;
    result
}

/// A control client: a reply for each request, and its input passed on.
async fn control(
    mut frames: mpsc::Receiver<io::Result<Frame>>,
    mut write: OwnedWriteHalf,
    events: &mpsc::Sender<Event>,
) -> io::Result<()> {
    while let Some(frame) = frames.recv().await {
        let frame = frame?;
        match frame.kind {
            FrameKind::Request => {
                let reply = match frame.decode::<Request>() {
                    Ok(request) => {
                        let (tx, rx) = oneshot::channel();
                        if events.send(Event::Request(request, tx)).await.is_err() {
                            return Ok(());
                        }
                        match rx.await {
                            Ok(reply) => reply,
                            Err(_) => return Ok(()),
                        }
                    }
                    Err(e) => Reply::Error(e.to_string()),
                };
                write_frame(&mut write, &Frame::json(FrameKind::Reply, &reply)).await?;
            }
            FrameKind::Input => {
                let _ = events.send(Event::Input(None, frame.payload)).await;
            }
            other => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("a control client sent a {other:?} frame"),
                ));
            }
        }
    }
    Ok(())
}
