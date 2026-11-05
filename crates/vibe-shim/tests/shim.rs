//! The `vibe-shim` binary: a real terminal, real commands, real clients.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use vibe_core::rpc::SessionExit;
use vibe_core::shim::{
    self, Connection, Frame, FrameKind, Hello, PROTOCOL, Record, Role, Size, control_hello,
};

const SHIM: &str = env!("CARGO_BIN_EXE_vibe-shim");
const WAIT: Duration = Duration::from_secs(20);

struct Shim {
    // Under /tmp, as a socket path must fit in 104 bytes on macOS.
    dir: tempfile::TempDir,
    socket: PathBuf,
    state: PathBuf,
    pid: i32,
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("shim")
        .tempdir_in("/tmp")
        .unwrap()
}

fn shim_command(dir: &Path, extra: &[&str], command: &[&str]) -> Command {
    let mut cmd = Command::new(SHIM);
    cmd.args(["--ref", "quiet-amber-otter", "--socket"])
        .arg(dir.join("run/quiet-amber-otter.sock"))
        .arg("--state")
        .arg(dir.join("agents/quiet-amber-otter"))
        .arg("--cwd")
        .arg(dir)
        .args(extra)
        .arg("--")
        .args(command)
        .env("PS1", "$ ")
        .env("ENV", "/dev/null");
    cmd
}

impl Shim {
    fn start(command: &[&str]) -> Shim {
        Shim::start_with(&[], command)
    }

    fn start_with(extra: &[&str], command: &[&str]) -> Shim {
        let dir = tmp();
        let cmd = shim_command(dir.path(), extra, command);
        Shim::launched(dir, cmd)
    }

    /// As [`Shim::start`], allowed `files` descriptors, its command too.
    fn start_limited(files: libc::rlim_t, command: &[&str]) -> Shim {
        let dir = tmp();
        let mut cmd = shim_command(dir.path(), &[], command);
        // SAFETY: only setrlimit, which is async-signal-safe, after the fork.
        unsafe {
            cmd.pre_exec(move || {
                let mut limit = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                limit.rlim_cur = files;
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Shim::launched(dir, cmd)
    }

    fn launched(dir: tempfile::TempDir, mut cmd: Command) -> Shim {
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "vibe-shim failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let pid = String::from_utf8(out.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        Shim {
            socket: dir.path().join("run/quiet-amber-otter.sock"),
            state: dir.path().join("agents/quiet-amber-otter"),
            dir,
            pid,
        }
    }

    fn control(&self) -> Connection {
        shim::connect(&self.socket, &control_hello("test"), WAIT).unwrap()
    }

    fn attach(&self, size: Option<Size>) -> Attach {
        let hello = Hello {
            protocol: PROTOCOL,
            role: Role::Attach,
            client: "test".into(),
            tty: Some("/dev/test".into()),
            size,
        };
        let conn = shim::connect(&self.socket, &hello, WAIT).unwrap();
        conn.stream
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        Attach {
            stream: conn.stream,
            output: String::new(),
            exit: None,
            buf: Vec::new(),
        }
    }

    fn alive(&self) -> bool {
        alive(self.pid)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join(shim::LOG_FILE)).unwrap_or_default()
    }

    /// Waits for the shim to exit, and returns its record.
    fn record(&self) -> Record {
        wait_until("the shim to exit", || !self.alive());
        let text = std::fs::read_to_string(self.state.join(shim::RECORD_FILE)).unwrap();
        serde_json::from_str(&text).unwrap()
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        if self.alive() {
            // SAFETY: a signal to the shim this test started.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
        let _ = &self.dir;
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// An attach client, collecting what the shim sends.
struct Attach {
    stream: UnixStream,
    output: String,
    exit: Option<SessionExit>,
    buf: Vec<u8>,
}

impl Attach {
    fn send(&mut self, kind: FrameKind, payload: &[u8]) {
        shim::write_frame(&mut self.stream, &Frame::new(kind, payload)).unwrap();
    }

    fn type_line(&mut self, line: &str) {
        self.send(FrameKind::Input, format!("{line}\r").as_bytes());
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let frame = Frame::json(FrameKind::Resize, &Size { rows, cols });
        shim::write_frame(&mut self.stream, &frame).unwrap();
    }

    /// Reads whatever has arrived. False once the shim has closed.
    fn pump(&mut self) -> bool {
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => panic!("{e}"),
        }
        while let Ok(Some(frame)) = shim::read_frame(&mut &self.buf[..]) {
            let len = shim::HEADER_LEN + frame.payload.len();
            self.buf.drain(..len);
            match frame.kind {
                FrameKind::Output => self
                    .output
                    .push_str(&String::from_utf8_lossy(&frame.payload)),
                FrameKind::Exit => self.exit = Some(frame.decode().unwrap()),
                other => panic!("an attach client got a {other:?} frame"),
            }
        }
        true
    }

    /// Waits until the output so far contains `text`.
    fn expect(&mut self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.output.contains(text) {
            assert!(
                Instant::now() < deadline,
                "never saw {text:?} in {:?}",
                self.output
            );
            assert!(
                self.pump(),
                "the shim closed before {text:?}: {:?}",
                self.output
            );
        }
    }

    /// Waits for the end of the session.
    fn expect_exit(&mut self) -> SessionExit {
        let deadline = Instant::now() + WAIT;
        while self.pump() {
            assert!(Instant::now() < deadline, "no exit: {:?}", self.output);
        }
        self.exit.clone().expect("an exit frame before the close")
    }

    fn clear(&mut self) {
        self.output.clear();
    }
}

#[test]
fn attach_detach_and_reattach_a_long_running_shell() {
    let shim = Shim::start(&["sh"]);
    let mut a = shim.attach(Some(Size { rows: 24, cols: 80 }));
    a.type_line("echo one-$((1+2))");
    a.expect("one-3");
    let status = shim.control().status().unwrap();
    assert_eq!(status.clients.len(), 1);
    assert_eq!(status.clients[0].tty.as_deref(), Some("/dev/test"));
    drop(a);

    // Detached: the shell carries on, with nobody attached.
    wait_until("the detach", || {
        shim.control().status().unwrap().clients.is_empty()
    });
    assert!(shim.alive());

    // Reattaching redraws the screen from the model, then streams.
    let mut b = shim.attach(Some(Size { rows: 24, cols: 80 }));
    b.expect("one-3");
    b.clear();
    b.type_line("echo two-$((2+2))");
    b.expect("two-4");
    drop(b);

    let mut c = shim.attach(None);
    c.expect("two-4");
    assert!(shim.log().contains("detached"), "{}", shim.log());
}

/// A Mac makes a pipe in two steps, so a shim started between them inherits the write
/// end. It's gone by the time the shim is up: whoever's reading that pipe would otherwise
/// wait for as long as the agent lasts.
#[test]
fn a_descriptor_it_inherited_is_kept_by_neither_the_shim_nor_its_command() {
    let (mut reader, writer) = std::io::pipe().unwrap();
    let dir = tmp();
    let mut cmd = shim_command(dir.path(), &[], &["sleep", "60"]);
    let fd = writer.as_raw_fd();
    // SAFETY: only dup2, which is async-signal-safe, after the fork. This copy stays
    // open across the exec, and only this shim gets it.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(fd, 40) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let shim = Shim::launched(dir, cmd);
    drop(writer);
    let (ended, end) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = ended.send(reader.read_to_end(&mut Vec::new()));
    });
    end.recv_timeout(WAIT)
        .expect("the pipe is still held open")
        .unwrap();
    assert!(shim.alive(), "{}", shim.log());
}

#[test]
fn clients_share_one_terminal() {
    let shim = Shim::start(&["sh"]);
    let mut a = shim.attach(None);
    let mut b = shim.attach(None);
    a.type_line("echo from-$((10+1))");
    a.expect("from-11");
    b.expect("from-11");
    // A control client's input reaches the terminal too, as vibe_send will.
    shim::write_frame(
        &mut shim.control().stream,
        &Frame::new(FrameKind::Input, "echo ctl-$((6*7))\r"),
    )
    .unwrap();
    a.expect("ctl-42");
    b.expect("ctl-42");
    assert_eq!(shim.control().status().unwrap().clients.len(), 2);
}

#[test]
fn the_shim_leads_the_session_and_its_death_hangs_up_the_command() {
    let shim = Shim::start(&["sleep", "1000"]);
    let mut control = shim.control();
    let welcome = control.welcome.clone();
    assert_eq!(welcome.pid as i32, shim.pid);
    assert_eq!(welcome.agent, "quiet-amber-otter");
    let child = welcome.child_pid as i32;
    // SAFETY: getsid and getpgid only read.
    unsafe {
        assert_eq!(libc::getsid(shim.pid), shim.pid, "the shim leads a session");
        assert_eq!(libc::getsid(child), shim.pid, "the command is in it");
        assert_eq!(libc::getpgid(child), child, "in a group of its own");
    }
    let status = control.status().unwrap();
    assert_eq!(status.command, ["sleep", "1000"]);
    assert_eq!(status.child_pid as i32, child);

    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim.pid, libc::SIGKILL) };
    wait_until("the command to get SIGHUP", || !alive(child));
}

#[test]
fn the_command_keeps_its_temp_files_in_the_state_directory() {
    let shim = Shim::start(&[
        "sh",
        "-c",
        "echo \"tmp=$TMPDIR\"; touch \"$TMPDIR/x\"; read y",
    ]);
    let tmp = shim.state.join("tmp");
    let mut a = shim.attach(None);
    a.expect(&format!("tmp={}", tmp.display()));
    wait_until("a temp file", || tmp.join("x").exists());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&tmp).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn an_exit_is_sent_to_clients_and_recorded() {
    let shim = Shim::start(&["sh", "-c", "read x; echo got-$x; exit 3"]);
    let mut a = shim.attach(None);
    a.type_line("yes");
    a.expect("got-yes");
    let exit = a.expect_exit();
    assert_eq!((exit.code, exit.signal.as_deref()), (Some(3), None));
    // The record is written before the socket goes, so a session is never
    // neither running nor exited.
    wait_until("the socket to go", || !shim.socket.exists());
    assert!(shim.state.join(shim::RECORD_FILE).exists());
    let record = shim.record();
    assert_eq!(record.exit, exit);
    assert_eq!(record.agent, "quiet-amber-otter");
    assert_eq!(record.command, ["sh", "-c", "read x; echo got-$x; exit 3"]);
    assert_eq!(record.cols, Some(shim::Size::DEFAULT.cols));
    assert!(
        record.output.screen.contains("got-yes"),
        "{:?}",
        record.output
    );
    assert!(!shim.socket.exists(), "the socket is removed");
    assert!(shim.log().contains("exited with code 3"), "{}", shim.log());
}

#[test]
fn stopping_the_shim_hangs_up_the_command() {
    let shim = Shim::start(&["sleep", "1000"]);
    let mut a = shim.attach(None);
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim.pid, libc::SIGTERM) };
    let exit = a.expect_exit();
    assert_eq!((exit.code, exit.signal.as_deref()), (None, Some("SIGHUP")));
    assert_eq!(shim.record().exit.signal.as_deref(), Some("SIGHUP"));
}

#[test]
fn a_command_that_ignores_the_hangup_is_killed() {
    let shim = Shim::start(&[
        "sh",
        "-c",
        "trap '' HUP; echo armed; while :; do sleep 1; done",
    ]);
    let mut a = shim.attach(None);
    a.expect("armed");
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim.pid, libc::SIGTERM) };
    let exit = a.expect_exit();
    assert_eq!(exit.signal.as_deref(), Some("SIGKILL"));
}

/// A command flooding its terminal, with a client that's stopped reading as a
/// window does when it closes, can't keep the shim from answering: `vibed` asks a
/// shim how it is before it does anything to a session, gives it 5 s, and takes
/// one slower than that for broken. The middle of several asks, so a slow moment
/// on a busy machine doesn't count against it.
#[test]
fn a_flood_of_output_doesnt_hold_up_requests() {
    let shim = Shim::start(&["yes", "spam"]);
    let stalled = shim.attach(Some(Size { rows: 24, cols: 80 }));
    // Long enough for a backlog, if one builds up.
    std::thread::sleep(Duration::from_secs(2));
    let mut asks = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        shim.control().status().unwrap();
        asks.push(started.elapsed());
    }
    asks.sort();
    assert!(asks[2] < Duration::from_secs(1), "{asks:?}");

    // And it's ended promptly, for all the output on its way: this allows for the
    // 2 s the stalled client is given to receive the end.
    let started = Instant::now();
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(shim.pid, libc::SIGTERM) };
    assert_eq!(shim.record().exit.signal.as_deref(), Some("SIGHUP"));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    drop(stalled);
}

/// A command that's told the test it has set its terminal raw, then stops, so
/// nothing reads its input until the test says.
fn stopped_on_a_raw_terminal(then: &str) -> Vec<String> {
    ["sh", "-c"]
        .map(String::from)
        .into_iter()
        .chain([format!("stty raw -echo; echo ready; kill -STOP $$; {then}")])
        .collect()
}

/// Lets a command from [`stopped_on_a_raw_terminal`] go on, once it has stopped.
fn carry_on(shim: &Shim) {
    let pid = shim.control().status().unwrap().child_pid.to_string();
    wait_until("the command to stop", || {
        Command::new("ps")
            .args(["-o", "state=", "-p", &pid])
            .output()
            .is_ok_and(|out| out.stdout.starts_with(b"T"))
    });
    // SAFETY: a signal to the command this test started.
    unsafe { libc::kill(pid.parse().unwrap(), libc::SIGCONT) };
}

/// A paste comes in as `vibe attach` reads it from a terminal, 4 KB at a time,
/// and it all waits for a command that's busy. What's been read makes room again:
/// the two together are more than may wait at once.
#[test]
fn a_big_paste_waits_for_a_command_that_isnt_reading() {
    const PASTE: usize = 20 << 20;
    let read = format!("head -c {PASTE} | wc -c");
    let command = stopped_on_a_raw_terminal(&format!("{read}; {read}"));
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let shim = Shim::start(&command);
    let mut a = shim.attach(None);
    a.expect("ready");
    for _ in 0..PASTE / 4096 {
        a.send(FrameKind::Input, &[b'x'; 4096]);
    }
    carry_on(&shim);
    a.expect(&PASTE.to_string());
    a.clear();
    for _ in 0..PASTE / 4096 {
        a.send(FrameKind::Input, &[b'x'; 4096]);
    }
    a.expect(&PASTE.to_string());
    assert!(!shim.log().contains("input"), "{}", shim.log());
}

/// There's a limit to what waits, and past it what's lost is said once, with how
/// much, however many chunks it comes to.
#[test]
fn input_past_the_limit_is_dropped_and_said_so_once() {
    let command = stopped_on_a_raw_terminal("cat >/dev/null");
    let command: Vec<&str> = command.iter().map(String::as_str).collect();
    let shim = Shim::start(&command);
    let mut a = shim.attach(None);
    a.expect("ready");
    let chunk = vec![b'x'; 1 << 20];
    for _ in 0..40 {
        a.send(FrameKind::Input, &chunk);
    }
    wait_until("the drop to be logged", || {
        shim.log().contains("isn't reading its input")
    });
    // Something small still fits, and ends the run of drops.
    wait_until("the total to be logged", || {
        a.send(FrameKind::Input, b"x");
        std::thread::sleep(Duration::from_millis(20));
        shim.log().contains("bytes of input")
    });
    let log = shim.log();
    assert_eq!(log.matches("isn't reading its input").count(), 1, "{log}");
    assert_eq!(log.matches("bytes of input").count(), 1, "{log}");
}

/// A shim out of descriptors can't take on clients, and its listener stays ready
/// to fail again; the ones it has go on seeing the command's output.
#[test]
fn running_out_of_descriptors_doesnt_stop_the_output() {
    let shim = Shim::start_limited(32, &["sh"]);
    let mut a = shim.attach(None);
    let hello = Frame::json(FrameKind::Hello, &control_hello("test"));
    let mut waiting = Vec::new();
    for _ in 0..64 {
        let mut stream = UnixStream::connect(&shim.socket).unwrap();
        shim::write_frame(&mut stream, &hello).unwrap();
        waiting.push(stream);
    }
    wait_until("the descriptors to run out", || {
        shim.log().contains("accept failed")
    });
    a.type_line("echo answer-$((40 + 2))");
    a.expect("answer-42");

    // And when there are descriptors again, it takes on clients again.
    drop(waiting);
    assert_eq!(shim.control().status().unwrap().clients.len(), 1);
}

/// Asks the shell its terminal's size, through client `via`.
fn stty_size(via: &mut Attach) -> String {
    // The shell works out the marker, so only its answer contains it: the
    // terminal's echo of the command line, which a shell may draw more than
    // once (bash redraws it on a resize), shows only `sz$((…))`. Each ask
    // has its own, so an answer redrawn from earlier isn't taken.
    static ASKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = ASKED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let marker = format!("sz{}=", 1000 + n);
    via.type_line(&format!("echo sz$((1000+{n}))=$(stty size | tr ' ' x)="));
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some((_, rest)) = via.output.split_once(&marker)
            && let Some((size, _)) = rest.split_once('=')
        {
            return size.to_owned();
        }
        assert!(Instant::now() < deadline, "no size in {:?}", via.output);
        assert!(via.pump());
    }
}

#[test]
fn the_client_active_last_sets_the_size() {
    let shim = Shim::start(&["sh"]);
    let mut a = shim.attach(Some(Size {
        rows: 30,
        cols: 100,
    }));
    assert_eq!(stty_size(&mut a), "30x100");
    let mut b = shim.attach(Some(Size { rows: 20, cols: 50 }));
    // Attaching makes b the active one.
    assert_eq!(stty_size(&mut b), "20x50");
    // Typing makes a active again.
    assert_eq!(stty_size(&mut a), "30x100");
    // So does resizing; and the screen model follows.
    b.resize(25, 60);
    wait_until("the resize", || {
        shim.control().status().unwrap().size == Size { rows: 25, cols: 60 }
    });
    assert_eq!(stty_size(&mut b), "25x60");
    // When b leaves, a's size comes back.
    drop(b);
    wait_until("the detach", || {
        shim.control().status().unwrap().size
            == Size {
                rows: 30,
                cols: 100,
            }
    });
    assert_eq!(stty_size(&mut a), "30x100");
}

#[test]
fn read_output_gives_the_screen_and_history_since_a_cursor() {
    let shim = Shim::start_with(&["--size", "40x6"], &["sh"]);
    let mut a = shim.attach(None);
    a.type_line("i=0; while [ $i -lt 12 ]; do i=$((i+1)); echo row-$i; done");
    a.expect("row-12");
    let mut control = shim.control();
    let first = control.read_output(None).unwrap();
    assert!(first.screen.contains("row-12"), "{first:?}");
    assert!(first.history.iter().any(|l| l == "row-1"), "{first:?}");
    assert!(first.cursor > 0);
    assert_eq!(first.dropped, 0);
    a.clear();
    a.type_line("echo last-$((5*5))");
    a.expect("last-25");
    let next = control.read_output(Some(first.cursor)).unwrap();
    assert!(
        !next.history.iter().any(|l| l == "row-1"),
        "only what's new: {next:?}"
    );
    assert!(next.cursor >= first.cursor);
    assert!(next.screen.contains("last-25"));
}

#[test]
fn a_second_shim_on_the_same_socket_is_refused() {
    let shim = Shim::start(&["sleep", "1000"]);
    let out = shim_command(shim.dir.path(), &[], &["sleep", "1000"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already running"), "{stderr}");
    assert!(
        shim.control().status().is_ok(),
        "the first one is untouched"
    );
}

#[test]
fn a_command_that_cant_start_fails_the_launch() {
    let dir = tmp();
    let out = shim_command(dir.path(), &[], &["/nonexistent/agent"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("can't start /nonexistent/agent"),
        "{stderr}"
    );
    assert!(!dir.path().join("run/quiet-amber-otter.sock").exists());
}

#[test]
fn clients_of_another_protocol_are_refused() {
    let shim = Shim::start(&["sleep", "1000"]);
    let mut hello = control_hello("future");
    hello.protocol = PROTOCOL + 1;
    let err = shim::connect(&shim.socket, &hello, WAIT).unwrap_err();
    match err {
        shim::ConnectError::Refused(r) => {
            assert_eq!(r.protocol, PROTOCOL);
            assert!(r.message.contains("protocol"), "{}", r.message);
        }
        other => panic!("{other}"),
    }
    // A newer client's hello may have fields this shim doesn't know; it
    // still learns the version rather than a decoding error.
    let mut s = UnixStream::connect(&shim.socket).unwrap();
    let newer = serde_json::json!({"protocol": PROTOCOL + 1, "role": "attach", "client": "x", "colour": true});
    shim::write_frame(&mut s, &Frame::json(FrameKind::Hello, &newer)).unwrap();
    let refused: shim::Refused = shim::read_frame(&mut s).unwrap().unwrap().decode().unwrap();
    assert!(
        refused.message.contains("speaks protocol"),
        "{}",
        refused.message
    );
    // Garbage instead of a hello is refused too, and the shim carries on.
    let mut s = UnixStream::connect(&shim.socket).unwrap();
    shim::write_frame(&mut s, &Frame::new(FrameKind::Input, "hi")).unwrap();
    let frame = shim::read_frame(&mut s).unwrap().unwrap();
    assert_eq!(frame.kind, FrameKind::Refused);
    assert!(shim.control().status().is_ok());
}

#[test]
fn the_record_of_a_previous_session_is_replaced() {
    let dir = tmp();
    let state = dir.path().join("agents/quiet-amber-otter");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join(shim::RECORD_FILE), "old").unwrap();
    let out = shim_command(dir.path(), &[], &["sleep", "1000"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let pid: i32 = String::from_utf8(out.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!state.join(shim::RECORD_FILE).exists());
    // SAFETY: a signal to the shim this test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };
}

#[test]
fn a_huge_size_is_ignored() {
    let shim = Shim::start(&["sh"]);
    let mut a = shim.attach(Some(Size {
        rows: 65535,
        cols: 65535,
    }));
    a.resize(60000, 60000);
    a.type_line("echo still-$((8*8))");
    a.expect("still-64");
    assert_eq!(shim.control().status().unwrap().size, Size::DEFAULT);
    assert!(shim.alive());
}

#[test]
fn a_long_history_is_read_in_pages_that_fit() {
    let shim = Shim::start_with(&["--size", "400x5"], &["sh"]);
    let mut a = shim.attach(None);
    // About 1.2 MB of history: more than a frame used to hold.
    a.type_line("yes $(printf %0390d 0) | head -n 3000; echo done-$((1+1))");
    a.expect("done-2");
    let mut control = shim.control();
    let mut after = None;
    let mut lines = 0;
    let mut bytes = 0;
    loop {
        let page = control.read_output(after).unwrap();
        lines += page.history.len();
        bytes += page.history.iter().map(String::len).sum::<usize>();
        after = Some(page.cursor);
        if !page.more {
            break;
        }
    }
    // All but the few still on the screen.
    assert!(lines >= 2990, "{lines} lines");
    assert!(bytes > 1 << 20, "{bytes} bytes");
}
