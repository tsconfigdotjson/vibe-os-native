//! The agent's command: one at a time, on a terminal the guest holds, in a
//! cgroup of its own, so it can be frozen whole and its processes listed,
//! however they were started (`nohup`, `setsid` and all).
//!
//! The guest keeps a screen model of the terminal, so whichever shim
//! connects next, after a dropped connection or a restore, is sent the
//! screen as it is and carries on from there. Output is read whether or not
//! a shim is connected, so the command never blocks on a missing one.

use std::collections::{BTreeMap, HashMap};
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::num::NonZeroU16;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use vibe_guest::proto::{
    self, ErrorCode, Exit, Failure, Frame, FrameKind, Frozen, GuestState, Launch, Launched,
    PROTOCOL, Process, Processes, Ran, Refused, Session, TermSize, TermWelcome, output_frames,
};

use crate::init::log;
use crate::init::sys::{self, check};

/// After the command exits, how long to wait for the rest of what it wrote
/// before telling the shim.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// How long a freeze or thaw may take to settle.
const FREEZE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a frame to a shim may take to send in all.
const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long leftovers of the last command get to die before a launch.
const CLEAR_TIMEOUT: Duration = Duration::from_secs(5);

/// Held while forking, and by the reaper while it reaps. A failed exec is
/// reaped by `Command::spawn` itself, which must not find it gone.
static SPAWN: Mutex<()> = Mutex::new(());

/// A lock that a panic elsewhere doesn't take down with it: PID 1 must
/// carry on.
pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Who runs the command, and in what.
#[derive(Debug)]
pub(crate) struct Settings {
    /// The uid and gid to switch to; `None` keeps the guest's own.
    pub(crate) user: Option<(u32, u32)>,
    /// Where a command starts unless told otherwise.
    pub(crate) home: PathBuf,
    /// The environment every command starts with.
    pub(crate) env: Vec<(String, String)>,
    /// The cgroup the command runs in, if the guest has one.
    pub(crate) cgroup: Option<PathBuf>,
}

/// A shim's connection to the terminal.
#[derive(Debug)]
pub(crate) struct Conn {
    pub(crate) id: u64,
    pub(crate) client: String,
    stream: File,
}

impl Conn {
    pub(crate) fn new(id: u64, client: String, stream: File) -> Conn {
        Conn { id, client, stream }
    }

    /// Sends a frame whole, or fails once it's been [`SEND_TIMEOUT`] at it, plus
    /// the write under way, which blocks at most the socket's send timeout.
    /// Output and the state wait on it, so a shim that takes that long over a
    /// frame is as gone as one that stopped.
    fn send(&self, frame: &Frame) -> io::Result<()> {
        let bytes = frame.encode();
        let deadline = Instant::now() + SEND_TIMEOUT;
        let mut sent = 0;
        while sent < bytes.len() {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the shim fell behind",
                ));
            }
            match (&self.stream).write(&bytes[sent..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => sent += n,
                // `WouldBlock` is the socket's send timeout, with nothing
                // taken: only the deadline says the shim has gone.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                    ) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn send_output(&self, bytes: &[u8]) -> io::Result<()> {
        for frame in output_frames(FrameKind::Output, bytes) {
            self.send(&frame)?;
        }
        Ok(())
    }

    /// Ends the connection both ways, which also wakes a thread blocked
    /// writing to it.
    pub(crate) fn shutdown(&self) {
        // SAFETY: shutdown on a socket we own; it has no memory effects.
        unsafe { libc::shutdown(self.stream.as_raw_fd(), libc::SHUT_RDWR) };
    }
}

pub(crate) struct Agent {
    settings: Settings,
    state: Mutex<State>,
    /// Signalled when a session's terminal closes.
    changed: Condvar,
    /// The attached connection, kept apart from `state` so that a new one
    /// can end it even while a write to it holds `state`.
    current: Mutex<Option<Arc<Conn>>>,
    next_conn: AtomicU64,
    /// Commands a `run` waits for, by pid: the reaper hands each its wait
    /// status, as it collects every child.
    runs: Mutex<HashMap<u32, mpsc::Sender<libc::c_int>>>,
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

struct State {
    session: Option<Running>,
    frozen: bool,
    generations: u64,
}

/// The command running now, or the last one, which keeps its screen.
struct Running {
    info: Session,
    generation: u64,
    master: Arc<File>,
    screen: vt100::Parser,
    size: TermSize,
    conn: Option<Arc<Conn>>,
    pty_closed: bool,
}

impl Running {
    /// Bytes that turn a fresh terminal into this one, as a shim redraws
    /// for a client that attaches.
    fn redraw(&self) -> Vec<u8> {
        let screen = self.screen.screen();
        let mut out = String::from(if screen.alternate_screen() {
            "\x1b[?1049h"
        } else {
            "\x1b[?1049l"
        });
        out.push_str(&screen.state_formatted());
        out.into_bytes()
    }

    /// Drops the connection, if it's `id` (any, if `None`).
    fn drop_conn(&mut self, id: Option<u64>) -> Option<Arc<Conn>> {
        if id.is_some() && self.conn.as_ref().map(|c| c.id) != id {
            return None;
        }
        let conn = self.conn.take()?;
        conn.shutdown();
        Some(conn)
    }
}

/// What an attached shim needs from then on.
#[derive(Debug)]
pub(crate) struct Attached {
    /// Whether to keep reading from it: false if the command had already
    /// exited, in which case it has been told so and closed.
    pub(crate) live: bool,
}

fn dims(size: TermSize) -> (NonZeroU16, NonZeroU16) {
    (
        NonZeroU16::new(size.rows).unwrap_or(NonZeroU16::MIN),
        NonZeroU16::new(size.cols).unwrap_or(NonZeroU16::MIN),
    )
}

fn failed(code: ErrorCode) -> impl FnOnce(io::Error) -> Failure {
    move |e| Failure::new(code, e.to_string())
}

impl Agent {
    pub(crate) fn new(settings: Settings) -> Agent {
        Agent {
            settings,
            state: Mutex::new(State {
                session: None,
                frozen: false,
                generations: 0,
            }),
            changed: Condvar::new(),
            current: Mutex::new(None),
            next_conn: AtomicU64::new(1),
            runs: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn next_conn_id(&self) -> u64 {
        self.next_conn.fetch_add(1, Ordering::SeqCst)
    }

    /// The state, whether the command is frozen, and its session.
    pub(crate) fn status(&self) -> (GuestState, bool, Option<Session>) {
        let state = lock(&self.state);
        let session = state.session.as_ref().map(|s| s.info.clone());
        let guest = match &session {
            None => GuestState::Idle,
            Some(s) if s.exit.is_none() => GuestState::Running,
            Some(_) => GuestState::Exited,
        };
        (guest, state.frozen, session)
    }

    pub(crate) fn launch(self: &Arc<Self>, launch: &Launch) -> Result<Launched, Failure> {
        let mut state = lock(&self.state);
        if let Some(s) = &state.session
            && s.info.exit.is_none()
        {
            return Err(Failure::new(
                ErrorCode::Busy,
                format!("a command is already running, as pid {}", s.info.pid),
            ));
        }
        if state.frozen {
            return Err(Failure::new(
                ErrorCode::Busy,
                "the agent is frozen; thaw it first",
            ));
        }
        if let Some(time_ms) = launch.time_ms {
            sys::set_clock(time_ms).map_err(|e| {
                Failure::new(ErrorCode::ClockFailed, format!("can't set the clock: {e}"))
            })?;
        }
        // What the last command left behind goes first, so that what's in
        // the cgroup is only ever this command's.
        self.clear_cgroup()
            .map_err(failed(ErrorCode::LaunchFailed))?;
        if let Some(old) = state.session.as_mut() {
            old.drop_conn(None);
        }
        lock(&self.current).take();

        let cwd = launch
            .cwd
            .as_deref()
            .map_or_else(|| self.settings.home.clone(), PathBuf::from);
        if !cwd.is_dir() {
            return Err(Failure::new(
                ErrorCode::LaunchFailed,
                format!("{} isn't a directory", cwd.display()),
            ));
        }
        let size = launch.size.unwrap_or(TermSize::DEFAULT);
        let (master, slave) =
            open_pty(size, self.settings.user).map_err(failed(ErrorCode::LaunchFailed))?;
        let procs = match &self.settings.cgroup {
            Some(dir) => Some(
                OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_CLOEXEC)
                    .open(dir.join("cgroup.procs"))
                    .map_err(|e| {
                        Failure::new(
                            ErrorCode::LaunchFailed,
                            format!("can't open the agent's cgroup: {e}"),
                        )
                    })?,
            ),
            None => None,
        };

        let mut env: BTreeMap<&str, &str> = BTreeMap::new();
        for (k, v) in &self.settings.env {
            env.insert(k, v);
        }
        for (k, v) in &launch.env {
            env.insert(k, v);
        }
        let mut cmd = Command::new(&launch.command[0]);
        cmd.args(&launch.command[1..])
            .env_clear()
            .envs(env)
            .stdin(Stdio::from(
                slave.try_clone().map_err(failed(ErrorCode::LaunchFailed))?,
            ))
            .stdout(Stdio::from(
                slave.try_clone().map_err(failed(ErrorCode::LaunchFailed))?,
            ))
            .stderr(Stdio::from(slave));
        let procs_fd = procs.as_ref().map(AsRawFd::as_raw_fd);
        let user = self.settings.user;
        let cwd_c = sys::cstr(&cwd).map_err(failed(ErrorCode::LaunchFailed))?;
        // SAFETY: only async-signal-safe calls between fork and exec, on
        // descriptors and values made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                // Into the agent's cgroup first, so nothing it starts is
                // ever outside it. "0" is the writer itself.
                if let Some(fd) = procs_fd
                    && libc::write(fd, b"0".as_ptr().cast(), 1) != 1
                {
                    return Err(io::Error::last_os_error());
                }
                // Its own session, with the terminal as its controlling
                // terminal, so job control and hangups work as in a login.
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                if let Some((uid, gid)) = user {
                    let groups = [gid];
                    check(libc::setgroups(1, groups.as_ptr()))?;
                    check(libc::setgid(gid))?;
                    check(libc::setuid(uid))?;
                }
                // As the user, so a directory it can't enter fails the
                // launch rather than starting it there anyway.
                check(libc::chdir(cwd_c.as_ptr()))?;
                Ok(())
            });
        }
        let child = {
            let _spawning = lock(&SPAWN);
            cmd.spawn().map_err(|e| {
                Failure::new(
                    ErrorCode::LaunchFailed,
                    format!("can't start {}: {e}", launch.command[0]),
                )
            })?
        };
        drop(cmd);
        drop(procs);
        let pid = child.id();
        // The reaper collects it; dropping a Child neither waits nor kills.
        drop(child);

        state.generations += 1;
        let generation = state.generations;
        let master = Arc::new(master);
        let (rows, cols) = dims(size);
        state.session = Some(Running {
            info: Session {
                pid,
                command: launch.command.clone(),
                cwd: cwd.to_string_lossy().into_owned(),
                started_ms: sys::uptime_ms(),
                exit: None,
            },
            generation,
            master: master.clone(),
            screen: vt100::Parser::new(rows, cols, 0),
            size,
            conn: None,
            pty_closed: false,
        });
        drop(state);
        let agent = self.clone();
        std::thread::Builder::new()
            .name("pty".into())
            .spawn(move || agent.read_pty(generation, master))
            .map_err(failed(ErrorCode::LaunchFailed))?;
        log!(
            "started {:?} in {} as pid {pid}",
            launch.command,
            cwd.display()
        );
        Ok(Launched { pid })
    }

    /// Feeds what the command writes to the screen and the shim, until its
    /// terminal closes.
    fn read_pty(&self, generation: u64, master: Arc<File>) {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match (&*master).read(&mut buf) {
                Ok(0) => break,
                Ok(n) => self.output(generation, &buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                // EIO: every slave descriptor is closed.
                Err(_) => break,
            }
        }
        let mut state = lock(&self.state);
        if let Some(s) = state.session.as_mut()
            && s.generation == generation
        {
            s.pty_closed = true;
        }
        drop(state);
        self.changed.notify_all();
    }

    fn output(&self, generation: u64, bytes: &[u8]) {
        let mut state = lock(&self.state);
        let Some(s) = state.session.as_mut() else {
            return;
        };
        if s.generation != generation {
            return;
        }
        s.screen.process(bytes);
        // Written with the lock held, so output, a redraw and the exit
        // reach the shim in order. A shim that stops reading is dropped
        // after `SEND_TIMEOUT`.
        if let Some(conn) = s.conn.clone()
            && let Err(e) = conn.send_output(bytes)
        {
            log!("dropped {} (connection {}): {e}", conn.client, conn.id);
            s.drop_conn(Some(conn.id));
        }
    }

    /// Runs `run` to completion beside the agent's command (`Request::Run`):
    /// as `vibe`, in a process group of its own, with stdout and stderr on
    /// one pipe. Past its time it's killed with its group, and whatever it
    /// leaves in the group is killed when it ends.
    pub(crate) fn run(&self, run: &proto::Run) -> Result<Ran, Failure> {
        run.validate()
            .map_err(|e| Failure::new(ErrorCode::BadRequest, e))?;
        let cwd = run
            .cwd
            .as_deref()
            .map_or_else(|| self.settings.home.clone(), PathBuf::from);
        let (mut reader, writer) = io::pipe().map_err(failed(ErrorCode::LaunchFailed))?;
        let mut env: BTreeMap<&str, &str> = BTreeMap::new();
        for (k, v) in &self.settings.env {
            env.insert(k, v);
        }
        for (k, v) in &run.env {
            env.insert(k, v);
        }
        let mut cmd = Command::new(&run.command[0]);
        cmd.args(&run.command[1..])
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .stdout(
                writer
                    .try_clone()
                    .map_err(failed(ErrorCode::LaunchFailed))?,
            )
            .stderr(writer);
        let user = self.settings.user;
        let cwd_c = sys::cstr(&cwd).map_err(failed(ErrorCode::LaunchFailed))?;
        // SAFETY: only async-signal-safe calls between fork and exec, on
        // values made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                check(libc::setpgid(0, 0))?;
                if let Some((uid, gid)) = user {
                    let groups = [gid];
                    check(libc::setgroups(1, groups.as_ptr()))?;
                    check(libc::setgid(gid))?;
                    check(libc::setuid(uid))?;
                }
                check(libc::chdir(cwd_c.as_ptr()))?;
                Ok(())
            });
        }
        let (tx, rx) = mpsc::channel();
        let pid = {
            // The reaper waits while this holds, so the pid is known before
            // the reaper can collect it.
            let _spawning = lock(&SPAWN);
            let child = cmd.spawn().map_err(|e| {
                Failure::new(
                    ErrorCode::LaunchFailed,
                    format!("can't start {}: {e}", run.command[0]),
                )
            })?;
            let pid = child.id();
            lock(&self.runs).insert(pid, tx);
            pid
        };
        // Our ends of the pipe go, so it ends once the command's do.
        drop(cmd);
        log!(
            "running {:?} in {} as pid {pid}",
            run.command,
            cwd.display()
        );
        let output = Arc::new(Mutex::new(Vec::new()));
        let (done_tx, done_rx) = mpsc::channel();
        {
            let output = Arc::clone(&output);
            std::thread::Builder::new()
                .name("run".into())
                .spawn(move || {
                    let mut buf = [0u8; 8192];
                    while let Ok(n) = reader.read(&mut buf) {
                        if n == 0 {
                            break;
                        }
                        let mut out = lock(&output);
                        out.extend_from_slice(&buf[..n]);
                        if out.len() > proto::MAX_RUN_OUTPUT {
                            let cut = out.len() - proto::MAX_RUN_OUTPUT;
                            out.drain(..cut);
                        }
                    }
                    let _ = done_tx.send(());
                })
                .map_err(failed(ErrorCode::LaunchFailed))?;
        }
        let group = pid as libc::pid_t;
        let kill_group = || {
            // SAFETY: killpg has no memory effects; an empty group fails.
            unsafe { libc::killpg(group, libc::SIGKILL) };
        };
        let mut timed_out = false;
        let status = match rx.recv_timeout(Duration::from_millis(run.timeout_ms)) {
            Ok(status) => Some(status),
            Err(_) => {
                timed_out = true;
                kill_group();
                rx.recv_timeout(Duration::from_secs(5)).ok()
            }
        };
        kill_group();
        lock(&self.runs).remove(&pid);
        let _ = done_rx.recv_timeout(Duration::from_secs(2));
        let exit = status.map(exit_of).unwrap_or(Exit {
            code: None,
            signal: None,
        });
        let output = String::from_utf8_lossy(&lock(&output)).into_owned();
        log!(
            "pid {pid} ran: {exit:?}{}",
            if timed_out { ", out of time" } else { "" }
        );
        Ok(Ran {
            code: exit.code,
            signal: exit.signal,
            timed_out,
            output,
        })
    }

    /// Called by the reaper for every child it collects.
    pub(crate) fn reaped(self: &Arc<Self>, pid: u32, status: libc::c_int) {
        if let Some(run) = lock(&self.runs).remove(&pid) {
            let _ = run.send(status);
            return;
        }
        let mut state = lock(&self.state);
        let Some(s) = state.session.as_mut() else {
            return;
        };
        if s.info.pid != pid || s.info.exit.is_some() {
            return;
        }
        let exit = exit_of(status);
        log!("the command (pid {pid}) ended: {exit:?}");
        s.info.exit = Some(exit);
        let generation = s.generation;
        drop(state);
        let agent = self.clone();
        let _ = std::thread::Builder::new()
            .name("exit".into())
            .spawn(move || agent.finish(generation));
    }

    /// Tells the shim how the command ended, once the rest of what it wrote
    /// is through, and closes the connection.
    fn finish(&self, generation: u64) {
        let deadline = Instant::now() + DRAIN_TIMEOUT;
        let mut state = lock(&self.state);
        loop {
            let Some(s) = state.session.as_ref() else {
                return;
            };
            if s.generation != generation {
                return;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if s.pty_closed || left.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let Some(s) = state.session.as_mut() else {
            return;
        };
        if let (Some(conn), Some(exit)) = (s.conn.clone(), s.info.exit.clone()) {
            let _ = conn.send(&Frame::json(FrameKind::Exit, &exit));
            s.drop_conn(Some(conn.id));
        }
    }

    /// Attaches a shim, taking over from the last one: it's welcomed and
    /// sent the screen, and the exit too if the command has ended. A size
    /// is applied first, so the screen it's sent fits it.
    pub(crate) fn attach(
        &self,
        conn: Arc<Conn>,
        size: Option<TermSize>,
    ) -> Result<Attached, String> {
        // End the last connection before waiting for the state: a write to
        // it may hold the state while it blocks.
        if let Some(old) = lock(&self.current).take() {
            old.shutdown();
        }
        let mut state = lock(&self.state);
        let Some(s) = state.session.as_mut() else {
            return Err("nothing is running in the guest yet".into());
        };
        if let Some(old) = s.drop_conn(None) {
            log!("{} (connection {}) was replaced", old.client, old.id);
        }
        if let Some(size) = size.filter(|z| z.is_valid() && *z != s.size) {
            resize(s, size);
        }
        let welcome = TermWelcome {
            protocol: PROTOCOL,
            session: s.info.clone(),
            size: s.size,
        };
        let sent = conn
            .send(&Frame::json(FrameKind::Welcome, &welcome))
            .and_then(|()| conn.send_output(&s.redraw()));
        if let Err(e) = sent {
            conn.shutdown();
            return Err(format!("couldn't send the screen: {e}"));
        }
        if let Some(exit) = &s.info.exit {
            let _ = conn.send(&Frame::json(FrameKind::Exit, exit));
            conn.shutdown();
            return Ok(Attached { live: false });
        }
        log!("{} attached (connection {})", conn.client, conn.id);
        s.conn = Some(conn.clone());
        *lock(&self.current) = Some(conn);
        Ok(Attached { live: true })
    }

    /// Input from connection `id`, if it's still the attached one.
    pub(crate) fn input(&self, id: u64, bytes: &[u8]) -> io::Result<()> {
        let master = {
            let state = lock(&self.state);
            match state.session.as_ref() {
                Some(s) if s.conn.as_ref().map(|c| c.id) == Some(id) && s.info.exit.is_none() => {
                    s.master.clone()
                }
                _ => return Ok(()),
            }
        };
        // Outside the lock: a command that isn't reading, a frozen one
        // say, blocks only this connection.
        (&*master).write_all(bytes)
    }

    pub(crate) fn resize(&self, id: u64, size: TermSize) {
        if !size.is_valid() {
            return;
        }
        let mut state = lock(&self.state);
        if let Some(s) = state.session.as_mut()
            && s.conn.as_ref().map(|c| c.id) == Some(id)
            && s.size != size
        {
            resize(s, size);
        }
    }

    pub(crate) fn detach(&self, id: u64) {
        let mut state = lock(&self.state);
        if let Some(s) = state.session.as_mut()
            && s.drop_conn(Some(id)).is_some()
        {
            log!("connection {id} detached");
        }
        drop(state);
        let mut current = lock(&self.current);
        if current.as_ref().map(|c| c.id) == Some(id) {
            current.take();
        }
    }

    /// The pids in the agent's cgroup, or without one, the command and its
    /// descendants.
    fn pids(&self) -> Vec<u32> {
        if let Some(dir) = &self.settings.cgroup {
            return fs::read_to_string(dir.join("cgroup.procs"))
                .unwrap_or_default()
                .lines()
                .filter_map(|l| l.trim().parse().ok())
                .collect();
        }
        let root = {
            let state = lock(&self.state);
            match state.session.as_ref() {
                Some(s) if s.info.exit.is_none() => s.info.pid,
                _ => return Vec::new(),
            }
        };
        descendants(root)
    }

    pub(crate) fn processes(&self) -> Processes {
        let ticks = sys::clock_ticks();
        let page = sys::page_size();
        let mut processes: Vec<Process> = self
            .pids()
            .into_iter()
            .filter_map(|pid| read_process(pid, ticks, page))
            .collect();
        processes.sort_by_key(|p| p.pid);
        Processes {
            uptime_ms: sys::uptime_ms(),
            processes,
        }
    }

    /// Freezes everything in the cgroup, then flushes every disk and share.
    pub(crate) fn freeze(&self) -> Result<Frozen, Failure> {
        let dir = self.cgroup_or_fail()?;
        let mut state = lock(&self.state);
        let t = Instant::now();
        if let Err(e) = set_frozen(dir, true) {
            // Half frozen is the worst of both: let it run again, and say so.
            let _ = set_frozen(dir, false);
            return Err(Failure::new(ErrorCode::FreezeFailed, e.to_string()));
        }
        state.frozen = true;
        let freeze_ms = t.elapsed().as_millis() as u64;
        drop(state);
        let t = Instant::now();
        // SAFETY: sync takes no arguments and can't fail.
        unsafe { libc::sync() };
        let frozen = Frozen {
            processes: self.pids().len() as u32,
            freeze_ms,
            sync_ms: t.elapsed().as_millis() as u64,
            memory: sys::memory(),
        };
        log!(
            "froze {} processes in {} ms, synced in {} ms",
            frozen.processes,
            frozen.freeze_ms,
            frozen.sync_ms
        );
        Ok(frozen)
    }

    /// Thaws the cgroup; how long it took.
    pub(crate) fn thaw(&self) -> Result<u64, Failure> {
        let dir = self.cgroup_or_fail()?;
        let mut state = lock(&self.state);
        let t = Instant::now();
        set_frozen(dir, false).map_err(failed(ErrorCode::FreezeFailed))?;
        state.frozen = false;
        let ms = t.elapsed().as_millis() as u64;
        log!("thawed in {ms} ms");
        Ok(ms)
    }

    fn cgroup_or_fail(&self) -> Result<&Path, Failure> {
        self.settings.cgroup.as_deref().ok_or_else(|| {
            Failure::new(
                ErrorCode::FreezeFailed,
                "the guest has no cgroup to freeze the agent with",
            )
        })
    }

    /// Kills everything left in the cgroup and waits for it to empty.
    fn clear_cgroup(&self) -> io::Result<()> {
        let Some(dir) = &self.settings.cgroup else {
            return Ok(());
        };
        if self.pids().is_empty() {
            return Ok(());
        }
        log!("killing what the last command left running");
        if fs::write(dir.join("cgroup.kill"), "1").is_err() {
            // Before Linux 5.14.
            for pid in self.pids() {
                // SAFETY: kill has no memory effects.
                unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        wait_until(CLEAR_TIMEOUT, || self.pids().is_empty())
            .then_some(())
            .ok_or_else(|| io::Error::other("the last command's processes won't die"))
    }

    /// Ends the command and everything it started: `SIGTERM`, then after
    /// `grace`, `SIGKILL`. For a poweroff.
    pub(crate) fn end_all(&self, grace: Duration) {
        if let Some(dir) = &self.settings.cgroup {
            let _ = set_frozen(dir, false);
        }
        for pid in self.pids() {
            // SAFETY: kill has no memory effects.
            unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        }
        if !wait_until(grace, || self.pids().is_empty()) {
            let _ = self.clear_cgroup();
        }
    }
}

fn resize(s: &mut Running, size: TermSize) {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the master is a terminal we own, and ws outlives the call.
    // The kernel sends SIGWINCH to the terminal's foreground.
    if unsafe { libc::ioctl(s.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } == -1 {
        log!(
            "can't resize the terminal to {size}: {}",
            io::Error::last_os_error()
        );
        return;
    }
    let (rows, cols) = dims(size);
    s.screen.set_size(rows, cols);
    s.size = size;
}

fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Freezes or thaws a cgroup and waits until the kernel says it has.
fn set_frozen(dir: &Path, frozen: bool) -> io::Result<()> {
    let want = if frozen { "1" } else { "0" };
    fs::write(dir.join("cgroup.freeze"), want)?;
    let settled = wait_until(FREEZE_TIMEOUT, || {
        fs::read_to_string(dir.join("cgroup.events"))
            .ok()
            .and_then(|e| {
                e.lines()
                    .find_map(|l| l.strip_prefix("frozen ").map(|v| v.trim() == want))
            })
            .unwrap_or(false)
    });
    if settled {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "the cgroup didn't {} within {}s",
            if frozen { "freeze" } else { "thaw" },
            FREEZE_TIMEOUT.as_secs()
        )))
    }
}

/// How a wait status reads: an exit code, or the signal that ended it.
pub(crate) fn exit_of(status: libc::c_int) -> Exit {
    if libc::WIFEXITED(status) {
        Exit {
            code: Some(libc::WEXITSTATUS(status)),
            signal: None,
        }
    } else if libc::WIFSIGNALED(status) {
        Exit {
            code: None,
            signal: Some(sys::signal_name(libc::WTERMSIG(status))),
        }
    } else {
        Exit {
            code: None,
            signal: None,
        }
    }
}

/// A new terminal of `size`, master and slave, neither inherited across an
/// exec, the slave owned by `user` as a login's would be.
fn open_pty(size: TermSize, user: Option<(u32, u32)>) -> io::Result<(File, OwnedFd)> {
    // SAFETY: each call is checked, and every descriptor owned at once.
    unsafe {
        let master = check(libc::posix_openpt(
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        ))?;
        let master = OwnedFd::from_raw_fd(master);
        check(libc::grantpt(master.as_raw_fd()))?;
        check(libc::unlockpt(master.as_raw_fd()))?;
        let mut name = [0 as libc::c_char; 128];
        let rc = libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len());
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        let name = CStr::from_ptr(name.as_ptr());
        let slave = check(libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        ))?;
        let slave = OwnedFd::from_raw_fd(slave);
        if let Some((uid, gid)) = user {
            check(libc::fchown(slave.as_raw_fd(), uid, gid))?;
            check(libc::fchmod(slave.as_raw_fd(), 0o620))?;
        }
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        check(libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws))?;
        Ok((File::from(master), slave))
    }
}

/// `pid` and every process below it, from `/proc`.
fn descendants(pid: u32) -> Vec<u32> {
    let mut parents: Vec<(u32, u32)> = Vec::new();
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let Some(p) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some((ppid, _)) = fs::read_to_string(format!("/proc/{p}/stat"))
                .ok()
                .and_then(|s| parse_stat(&s).map(|st| (st.ppid, st)))
            {
                parents.push((p, ppid));
            }
        }
    }
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        found.extend(
            parents
                .iter()
                .filter(|(_, pp)| *pp == parent)
                .map(|(p, _)| *p),
        );
        i += 1;
    }
    found
}

/// The fields of `/proc/<pid>/stat` the report uses.
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    name: String,
    state: String,
    ppid: u32,
    /// User and system time, in clock ticks.
    cpu: u64,
    /// Since boot, in clock ticks.
    start: u64,
    /// In pages.
    rss: u64,
}

/// The name is in parentheses and may hold anything, spaces and
/// parentheses included, so the fields after it are found from the last
/// `)`.
fn parse_stat(text: &str) -> Option<Stat> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let name = text.get(open + 1..close)?.to_owned();
    let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // `rest[0]` is field 3 of proc(5).
    let field = |n: usize| rest.get(n - 3).copied();
    let num = |n: usize| field(n).and_then(|f| f.parse::<u64>().ok());
    Some(Stat {
        name,
        state: field(3)?.to_owned(),
        ppid: num(4)? as u32,
        cpu: num(14)? + num(15)?,
        start: num(22)?,
        rss: num(24)?,
    })
}

fn read_process(pid: u32, ticks: u64, page: u64) -> Option<Process> {
    let stat = parse_stat(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)?;
    let args = fs::read(format!("/proc/{pid}/cmdline"))
        .unwrap_or_default()
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some(Process {
        pid,
        ppid: stat.ppid,
        name: stat.name,
        args,
        state: stat.state,
        started_ms: stat.start * 1000 / ticks,
        cpu_ms: stat.cpu * 1000 / ticks,
        rss: stat.rss * page,
    })
}

/// Reaps every child for ever, the orphans PID 1 inherits included, and
/// tells the agent about each. SIGCHLD must be blocked in every thread.
pub(crate) fn reap(agent: Arc<Agent>) -> ! {
    // SAFETY: a sigset on the stack, filled before use.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
    }
    let tick = libc::timespec {
        tv_sec: 1,
        tv_nsec: 0,
    };
    loop {
        // SAFETY: waits for SIGCHLD, which is blocked, or a second; either
        // way the loop below reaps whatever is there.
        unsafe { libc::sigtimedwait(&set, std::ptr::null_mut(), &tick) };
        let mut gone = Vec::new();
        {
            let _spawning = lock(&SPAWN);
            loop {
                let mut status = 0;
                // SAFETY: status is ours for waitpid to fill.
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid <= 0 {
                    break;
                }
                gone.push((pid as u32, status));
            }
        }
        for (pid, status) in gone {
            agent.reaped(pid, status);
        }
    }
}

/// Blocks SIGCHLD in this thread and every thread it starts from now on,
/// for [`reap`]. Commands get the default mask back when they start.
pub(crate) fn block_sigchld() {
    // SAFETY: a sigset on the stack, filled before use.
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGCHLD);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
}

/// Sets a socket's send and receive timeouts; `None` waits for ever.
pub(crate) fn set_timeouts(
    fd: RawFd,
    read: Option<Duration>,
    write: Option<Duration>,
) -> io::Result<()> {
    for (opt, t) in [(libc::SO_RCVTIMEO, read), (libc::SO_SNDTIMEO, write)] {
        let t = t.unwrap_or_default();
        let tv = libc::timeval {
            tv_sec: t.as_secs() as _,
            tv_usec: t.subsec_micros() as _,
        };
        // SAFETY: tv outlives the call, and its size is what's passed.
        check(unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                (&raw const tv).cast(),
                size_of::<libc::timeval>() as libc::socklen_t,
            )
        })?;
    }
    Ok(())
}

/// What to say to a shim that can't attach.
pub(crate) fn refusal(message: String) -> Frame {
    Frame::json(
        FrameKind::Refused,
        &Refused {
            message,
            protocol: PROTOCOL,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_lines_with_odd_names() {
        let line = "42 (we (ird) name) S 1 42 42 0 -1 4194560 100 0 0 0 7 3 0 0 20 0 1 0 \
                    1234 1000000 250 18446744073709551615";
        assert_eq!(
            parse_stat(line),
            Some(Stat {
                name: "we (ird) name".into(),
                state: "S".into(),
                ppid: 1,
                cpu: 10,
                start: 1234,
                rss: 250,
            })
        );
        assert_eq!(parse_stat("42 (x) S 1"), None);
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn this_process_reads_back() {
        let me = std::process::id();
        let p = read_process(me, sys::clock_ticks(), sys::page_size()).unwrap();
        assert_eq!(p.pid, me);
        assert!(p.rss > 0 && !p.args.is_empty(), "{p:?}");
        assert!(p.started_ms <= sys::uptime_ms());
        assert!(descendants(1).contains(&me));
    }

    #[test]
    fn exits_read_as_codes_or_signal_names() {
        assert_eq!(exit_of(3 << 8).code, Some(3));
        let killed = exit_of(libc::SIGKILL);
        assert_eq!(
            (killed.code, killed.signal.as_deref()),
            (None, Some("SIGKILL"))
        );
    }
}
