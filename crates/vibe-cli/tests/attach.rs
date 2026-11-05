//! `vibe attach` and `vibe ls` on a real terminal, against an in-process
//! `vibed` and real shims.

// vibed's test helpers: shims, records and short temp dirs.
#[path = "../../vibed/tests/common/mod.rs"]
mod common;

use std::ffi::CStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use common::{RunningShim, alive, short_tempdir, start_shim, wait_until, write_record};
use vibe_core::paths::Layout;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibe_core::shim::{self, Size, control_hello};
use vibed::core::{Config, Core, DaemonInfo};
use vibed::server::{self, SocketGuard};

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");
const WAIT: Duration = Duration::from_secs(20);

/// A home with sessions in it, and a vibed serving it.
struct World {
    layout: Layout,
    socket: PathBuf,
    reaper: Child,
    _guard: SocketGuard,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}

/// A test that fails leaves its shims behind otherwise, with their shells still
/// at whatever they were running, spinning for as long as the machine is up. A
/// process of its own ends them when this one's end of a pipe closes, which it
/// does however this ends, a `SIGKILL` included. Found by the command line, as a
/// shim whose output nobody reads may not answer on its socket, whose path is
/// this world's alone. The shells go with their terminals.
fn reaper(layout: &Layout) -> Child {
    Command::new("sh")
        .args([
            "-c",
            "trap '' HUP INT TERM; cat >/dev/null; exec pkill -KILL -f \"$SHIMS\"",
        ])
        // Not in its arguments, or it would be found among them.
        .env("SHIMS", format!("socket {}/", layout.run_dir().display()))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

impl Drop for World {
    fn drop(&mut self) {
        drop(self.reaper.stdin.take());
        // Not forever, should a process started just then have inherited the pipe.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !matches!(self.reaper.try_wait(), Ok(Some(_)) | Err(_)) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn world() -> World {
    let dir = short_tempdir();
    let home = dir.path().canonicalize().unwrap().join("home");
    let layout = Layout::new(&home, std::env::consts::OS);
    let mut config = Config::new(DaemonInfo::current(), layout.clone());
    config.gh = "/nonexistent/gh".into();
    let core = Arc::new(Core::open(config).unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let socket = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&socket).unwrap();
    rt.spawn(async move {
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        server::serve(listener, core, MAX_MESSAGE_BYTES, std::future::pending()).await;
    });
    World {
        reaper: reaper(&layout),
        layout,
        socket,
        _guard: guard,
        _rt: rt,
        _dir: dir,
    }
}

impl World {
    fn shell(&self) -> RunningShim {
        self.session(&["sh"])
    }

    fn session(&self, command: &[&str]) -> RunningShim {
        start_shim(&self.layout, "quiet-amber-otter", command)
    }

    fn vibe(&self) -> Command {
        let mut cmd = Command::new(VIBE);
        cmd.env("VIBE_SOCKET", &self.socket)
            .env("HOME", self.layout.home())
            // Not over SSH, even if these tests are run that way: a hangup
            // means something else there.
            .env_remove("SSH_CONNECTION")
            .env_remove("SSH_TTY");
        cmd
    }

    /// `vibe attach <agent>` on a terminal of `size`.
    fn attach(&self, agent: &str, size: Size) -> Term {
        let mut cmd = self.vibe();
        cmd.args(["attach", agent]);
        Term::spawn(cmd, size)
    }
}

/// A terminal with a program on it, as Terminal.app would run it: the
/// program leads a session whose controlling terminal this is.
struct Term {
    master: Option<File>,
    /// Kept open, as a terminal emulator would, until the window closes.
    slave: Option<OwnedFd>,
    child: Child,
    seen: Arc<Mutex<Vec<u8>>>,
    /// Tells the reader to stop reading, after what it's reading now.
    stall: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

fn check(rc: libc::c_int) -> std::io::Result<libc::c_int> {
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn set_size(fd: libc::c_int, size: Size) {
    let ws = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: ws outlives the call.
    check(unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as _, &ws) }).unwrap();
}

impl Term {
    fn spawn(mut cmd: Command, size: Size) -> Term {
        // SAFETY: each call is checked, and the descriptors owned at once.
        let (master, slave) = unsafe {
            let m = OwnedFd::from_raw_fd(
                check(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY)).unwrap(),
            );
            check(libc::grantpt(m.as_raw_fd())).unwrap();
            check(libc::unlockpt(m.as_raw_fd())).unwrap();
            let name = CStr::from_ptr(libc::ptsname(m.as_raw_fd())).to_owned();
            let s = OwnedFd::from_raw_fd(
                check(libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY)).unwrap(),
            );
            // Kept from the program, or closing the window wouldn't close
            // the terminal: the program would hold both ends itself.
            for fd in [&m, &s] {
                check(libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC)).unwrap();
            }
            (m, s)
        };
        set_size(master.as_raw_fd(), size);
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave.try_clone().unwrap()));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                check(libc::setsid())?;
                check(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        let mut term = Term {
            master: Some(File::from(master)),
            slave: Some(slave),
            child,
            seen: Arc::new(Mutex::new(Vec::new())),
            stall: Arc::new(AtomicBool::new(false)),
            reader: None,
        };
        term.start_reading();
        term
    }

    fn start_reading(&mut self) {
        let mut reader = self.master().try_clone().unwrap();
        let sink = self.seen.clone();
        let stalled = self.stall.clone();
        stalled.store(false, Ordering::SeqCst);
        self.reader = Some(std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
                if stalled.load(Ordering::SeqCst) {
                    break;
                }
            }
        }));
    }

    fn master(&self) -> &File {
        self.master.as_ref().expect("the window is open")
    }

    /// Closes the window for real, as an emulator does: it stops reading
    /// while the program is still writing, so the program blocks with the
    /// terminal's buffer full, and then every end of the terminal this
    /// holds closes. The kernel hangs the terminal up, so the program's
    /// write fails as its `SIGHUP` arrives. Needs output running: the
    /// reader stops only once a read returns.
    fn close_window_while_writing(&mut self) {
        self.stop_reading();
        self.close_window();
    }

    /// Stops reading while the program is still writing, so it blocks with the
    /// terminal's buffer full, as with a window that's stuck or about to close.
    fn stop_reading(&mut self) {
        self.stall.store(true, Ordering::SeqCst);
        self.reader.take().unwrap().join().unwrap();
        std::thread::sleep(Duration::from_millis(500));
    }

    fn close_window(&mut self) {
        self.master = None;
        self.slave = None;
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.seen.lock().unwrap()).into_owned()
    }

    fn expect(&self, text: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.text().contains(text) {
            assert!(
                Instant::now() < deadline,
                "never saw {text:?} in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn type_bytes(&mut self, bytes: &[u8]) {
        self.master().write_all(bytes).unwrap();
    }

    fn type_line(&mut self, line: &str) {
        self.type_bytes(format!("{line}\r").as_bytes());
    }

    fn resize(&self, size: Size) {
        set_size(self.master().as_raw_fd(), size);
    }

    /// Waits for the program to exit, and returns its exit code.
    fn wait(&mut self) -> Option<i32> {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status.code();
            }
            assert!(
                Instant::now() < deadline,
                "it didn't exit: {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits until `vibe attach` has joined the session: it names the window
    /// once the shim has taken it on. Typing before then would be echoed by
    /// this terminal itself, not by the session.
    fn attached(&self) {
        self.expect("\x1b]0;");
    }

    /// Whether the terminal is back in canonical mode with echo, as a shell
    /// left it. Read through the master: macOS revokes the slave side once
    /// the session leader that owned it exits.
    fn cooked(&self) -> bool {
        // SAFETY: termios is plain data, filled in by tcgetattr.
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            check(libc::tcgetattr(self.master().as_raw_fd(), &mut t)).unwrap();
            t.c_lflag & libc::ICANON != 0 && t.c_lflag & libc::ECHO != 0
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        // Not forever: a program exiting with output nobody reads waits for
        // its terminal to drain, which this must not hang the tests on.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && !matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn clients(w: &World) -> usize {
    let socket = w.layout.session_socket("quiet-amber-otter");
    shim::connect(&socket, &control_hello("t"), WAIT)
        .unwrap()
        .status()
        .unwrap()
        .clients
        .len()
}

/// Asks the session's shell for its terminal size. The shell works out the
/// marker, so only its answer contains it, never the echo of the command
/// line, which bash may draw more than once (it redraws it on a resize).
/// `n` makes each ask's marker its own.
fn stty_size(term: &mut Term, n: u32) -> String {
    let marker = format!("sz{}=", 1000 + n);
    term.type_line(&format!("echo sz$((1000+{n}))=$(stty size | tr ' ' x)="));
    let deadline = Instant::now() + WAIT;
    loop {
        let text = term.text();
        if let Some((_, rest)) = text.split_once(&marker)
            && let Some((size, _)) = rest.split_once('=')
        {
            return size.to_owned();
        }
        assert!(Instant::now() < deadline, "no size in {text:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn attach_streams_resizes_and_detaches_with_ctrl_backslash() {
    let w = world();
    let shim = w.shell();
    let mut term = w.attach(
        "quiet-amber-otter",
        Size {
            rows: 30,
            cols: 100,
        },
    );
    // The window is named for the session.
    term.expect("\x1b]0;quiet-amber-otter\x07");
    term.type_line("echo one-$((1+2))");
    term.expect("one-3");
    assert!(!term.cooked(), "raw while attached");
    assert_eq!(clients(&w), 1);

    // Raw: Ctrl-C goes to the session, not to vibe attach.
    term.type_line("sleep 100");
    std::thread::sleep(Duration::from_millis(200));
    term.type_bytes(b"\x03");
    term.type_line("echo after-$((2+5))");
    term.expect("after-7");
    assert!(term.child.try_wait().unwrap().is_none());

    // The session takes the window's size, and follows it.
    assert_eq!(stty_size(&mut term, 1), "30x100");
    term.resize(Size {
        rows: 40,
        cols: 120,
    });
    wait_until("the resize", || {
        shim::connect(&shim.socket, &control_hello("t"), WAIT)
            .unwrap()
            .status()
            .unwrap()
            .size
            == Size {
                rows: 40,
                cols: 120,
            }
    });
    assert_eq!(stty_size(&mut term, 2), "40x120");

    // Ctrl-\ detaches, puts the terminal back, and the session goes on.
    term.type_bytes(b"\x1c");
    assert_eq!(term.wait(), Some(0));
    term.expect("[detached from quiet-amber-otter]");
    assert!(term.cooked(), "the terminal is put back");
    assert!(alive(shim.pid));
    wait_until("the detach", || clients(&w) == 0);

    // Reattaching redraws what's on the screen.
    let mut again = w.attach(
        "quiet-amber-otter",
        Size {
            rows: 40,
            cols: 120,
        },
    );
    again.expect("after-7");
    again.type_line("echo two-$((4*4))");
    again.expect("two-16");
}

#[test]
fn closing_the_window_detaches() {
    let w = world();
    let shim = w.shell();
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("echo up-$((3+3))");
    term.expect("up-6");
    // Closing Terminal's window hangs up its terminal, and the kernel
    // sends the session leader, vibe attach, SIGHUP.
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
    term.wait();
    wait_until("the detach", || clients(&w) == 0);
    assert!(alive(shim.pid), "the session goes on");
}

#[test]
fn a_hangup_detaches_even_when_hangups_were_ignored() {
    let w = world();
    let shim = w.shell();
    let mut cmd = w.vibe();
    cmd.args(["attach", "quiet-amber-otter"]);
    // As when whoever started it ignored SIGHUP, which children inherit.
    // SAFETY: signal is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
            Ok(())
        });
    }
    let mut term = Term::spawn(cmd, Size { rows: 24, cols: 80 });
    term.attached();
    assert_eq!(clients(&w), 1, "counted as attached once it's welcomed");
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
    term.wait();
    wait_until("the detach", || clients(&w) == 0);
    assert!(alive(shim.pid));
}

#[test]
fn the_programs_own_title_is_kept_out() {
    let w = world();
    let _shim = w.shell();
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line(r"printf '\033]0;stolen\007shown-%s\n' $((9*9))");
    term.expect("shown-81");
    assert!(!term.text().contains("\x1b]0;stolen"), "{:?}", term.text());
}

#[test]
fn an_exit_is_reported_and_ends_the_attachment() {
    let w = world();
    let _shim = w.session(&["sh", "-c", "read x; exit 7"]);
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("go");
    assert_eq!(term.wait(), Some(0));
    term.expect("[quiet-amber-otter exited with code 7]");
    assert!(term.cooked());
}

#[test]
fn attach_works_while_vibed_is_down() {
    let w = world();
    let _shim = w.shell();
    let mut cmd = Command::new(VIBE);
    cmd.args(["attach", "quiet-amber-otter"])
        .env("VIBE_SOCKET", w.layout.home().join("no-vibed.sock"))
        .env("HOME", w.layout.home());
    let mut term = Term::spawn(cmd, Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("echo direct-$((6+6))");
    term.expect("direct-12");
    term.type_bytes(b"\x1c");
    assert_eq!(term.wait(), Some(0));
}

#[test]
fn attach_explains_what_it_cant_do() {
    let w = world();
    // No terminal.
    let out = w
        .vibe()
        .args(["attach", "quiet-amber-otter"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("needs a terminal"), "{stderr}");
    assert!(stderr.contains("ssh -t"), "{stderr}");

    // No such session, and a session that ended.
    let mut term = w.attach("lone-grey-wolf", Size { rows: 24, cols: 80 });
    assert_eq!(term.wait(), Some(1));
    term.expect("no session lone-grey-wolf on local");
    write_record(
        &w.layout,
        "brave-copper-lynx",
        Some(2),
        None,
        "2026-10-30T11:00:00Z",
    );
    let mut term = w.attach("brave-copper-lynx", Size { rows: 24, cols: 80 });
    assert_eq!(term.wait(), Some(1));
    term.expect("its command exited with code 2");
    let mut term = w.attach("../etc", Size { rows: 24, cols: 80 });
    assert_eq!(term.wait(), Some(1));
    term.expect("isn't a valid ref");
}

#[test]
fn ls_lists_sessions() {
    let w = world();
    let out = w.vibe().arg("ls").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "no sessions\n");

    let _shim = w.shell();
    write_record(
        &w.layout,
        "brave-copper-lynx",
        None,
        Some("SIGKILL"),
        "2026-10-30T11:00:00Z",
    );
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("echo ready-$((1+1))");
    term.expect("ready-2");
    let out = w.vibe().arg("ls").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines[0],
        "brave-copper-lynx  exited   SIGKILL at 2026-10-30T11:00:00Z"
    );
    assert!(
        lines[1].starts_with("quiet-amber-otter  running  1 attached (/dev/"),
        "{stdout}"
    );

    let out = Command::new(VIBE)
        .arg("ls")
        .env("VIBE_SOCKET", Path::new("/nonexistent/vibed.sock"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("is vibed running?"));
}

/// A vibed with the test toolbox, whose shell is `sh`, and its supervisor,
/// which saves the toolbox when the shell exits.
fn toolbox_world() -> World {
    toolbox_world_with(|toolbox| toolbox)
}

fn toolbox_world_with(
    adjust: impl FnOnce(vibe_env::testing::TestToolbox) -> vibe_env::testing::TestToolbox,
) -> World {
    let dir = short_tempdir();
    let home = dir.path().canonicalize().unwrap().join("home");
    let layout = Layout::new(&home, std::env::consts::OS);
    let base = layout.linux_dir().join("base-1");
    std::fs::create_dir_all(&base).unwrap();
    for f in ["Image", "initrd", "base.img"] {
        std::fs::write(base.join(f), f).unwrap();
    }
    let mut config = Config::new(DaemonInfo::current(), layout.clone());
    config.gh = "/nonexistent/gh".into();
    config.shim = common::workspace_bin("vibe-shim").into_os_string();
    config.toolbox = Some(Arc::new(adjust(vibe_env::testing::TestToolbox::new(
        layout.clone(),
        vec!["sh".into()],
    ))));
    let core = Arc::new(Core::open(config).unwrap());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let socket = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&socket).unwrap();
    let supervised = core.clone();
    rt.spawn(async move {
        supervised.spawn_toolbox(Duration::from_millis(100), Duration::from_secs(3600));
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        server::serve(listener, core, MAX_MESSAGE_BYTES, std::future::pending()).await;
    });
    World {
        reaper: reaper(&layout),
        layout,
        socket,
        _guard: guard,
        _rt: rt,
        _dir: dir,
    }
}

#[test]
fn the_toolbox_shell_detaches_open_and_saves_on_exit() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    let toolbox = || {
        let mut cmd = w.vibe();
        cmd.arg("toolbox");
        Term::spawn(cmd, size)
    };
    let mut term = toolbox();
    term.expect("Exit the shell to save");
    term.attached();
    term.type_line("echo in-the-$((6*7))-toolbox");
    term.expect("in-the-42-toolbox");
    // Ctrl-\ leaves it open.
    term.type_bytes(b"\x1c");
    assert_eq!(term.wait(), Some(0));
    term.expect("The toolbox is still open");
    assert!(!w.layout.linux_dir().join("template-1.img").exists());

    // Opening again attaches to the same shell; exiting it saves.
    let mut term = toolbox();
    term.attached();
    term.type_line("echo $((6*7))-again");
    term.expect("42-again");
    term.type_line("exit");
    assert_eq!(term.wait(), Some(0), "{}", term.text());
    term.expect("Saved template 1 and Linux home 1");
    assert!(term.cooked());
    for f in ["template-1.img", "template-1.base", "home-1.img"] {
        assert!(w.layout.linux_dir().join(f).exists(), "{f}");
    }

    let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("state     closed\ntemplate  1\nhome      1\nbase      1\n"),
        "{text}"
    );
    assert!(
        text.contains("The toolbox shell saved template 1"),
        "{text}"
    );

    // It needs a terminal, and says so before booting anything.
    let out = w
        .vibe()
        .arg("toolbox")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a terminal"));
    // An update with no toolbox.toml says what's missing.
    let out = w.vibe().args(["toolbox", "update"]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("has no [update] commands"),
        "{out:?}"
    );
}

/// Closing the toolbox's window saves and closes it, as exiting does, so
/// it isn't left open with no terminal (#54); unless another terminal still
/// holds the shell. An agent's window still only detaches
/// (`closing_the_window_detaches`), and so does Ctrl-\ here.
#[test]
fn closing_the_toolbox_window_saves_it_unless_another_terminal_holds_it() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    let toolbox = || {
        let mut cmd = w.vibe();
        cmd.arg("toolbox");
        Term::spawn(cmd, size)
    };
    let template = w.layout.linux_dir().join("template-1.img");
    let state = || {
        let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let mut first = toolbox();
    first.attached();
    first.type_line("echo $((6*7))-first");
    first.expect("42-first");
    let mut second = toolbox();
    second.attached();
    // The first window closes: the second still has the shell.
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(first.child.id() as libc::pid_t, libc::SIGHUP) };
    assert_eq!(first.wait(), Some(0));
    assert!(!template.exists(), "nothing saved under the other window");
    assert!(state().starts_with("state     open\n"), "{}", state());
    second.type_line("echo $((6*7))-second");
    second.expect("42-second");
    // The last one closes: saved, as an exit is.
    // SAFETY: as above.
    unsafe { libc::kill(second.child.id() as libc::pid_t, libc::SIGHUP) };
    assert_eq!(second.wait(), Some(0), "{}", second.text());
    assert!(template.exists(), "{}", state());
    let text = state();
    assert!(text.starts_with("state     closed\n"), "{text}");
    assert!(
        text.contains("The toolbox shell saved template 1"),
        "{text}"
    );

    // Ctrl-\ still leaves it open, in the background, until Save & Close.
    let mut term = toolbox();
    term.attached();
    term.type_bytes(b"\x1c");
    assert_eq!(term.wait(), Some(0));
    term.expect("`vibe toolbox close`, to save");
    wait_until("the detach", || state().contains("no terminal attached"));
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Saved template 2 and Linux home 2"),
        "{out:?}"
    );
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("isn't open: there's nothing to save"),
        "{out:?}"
    );
}

/// The shell writes until the terminal is gone, then the window closes for
/// real: the failed write is a hangup like the `SIGHUP`, whichever comes
/// first, and the toolbox is saved all the same (#54).
#[test]
fn a_real_close_with_output_running_saves_the_toolbox() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    for version in 1..=3 {
        let mut cmd = w.vibe();
        cmd.arg("toolbox");
        let mut term = Term::spawn(cmd, size);
        term.attached();
        term.type_line("while :; do echo spam; done");
        term.expect("spam\r\nspam");
        term.close_window_while_writing();
        assert_eq!(term.wait(), Some(0), "{}", term.text());
        assert!(
            w.layout
                .linux_dir()
                .join(format!("template-{version}.img"))
                .exists(),
            "round {version}"
        );
        let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.starts_with("state     closed\n"), "{text}");
    }
}

/// `yes`, with a terminal attached: closing the window saves, and so does Save &
/// Close, which is `vibe toolbox close` here and the button in Settings. The shell
/// has to pass the hang-up on, as job control has `yes` in a group of its own.
#[test]
fn closing_with_yes_running_saves_the_toolbox() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    let toolbox = || {
        let mut cmd = w.vibe();
        cmd.arg("toolbox");
        let mut term = Term::spawn(cmd, size);
        term.attached();
        term.type_line("yes");
        term.expect("y\r\ny");
        term
    };
    let saved = |version: u32| {
        w.layout
            .linux_dir()
            .join(format!("template-{version}.img"))
            .exists()
    };

    let mut term = toolbox();
    term.close_window_while_writing();
    assert_eq!(term.wait(), Some(0), "{}", term.text());
    assert!(saved(1));

    let mut term = toolbox();
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("Saved template 2 and Linux home 2"),
        "{out:?}"
    );
    assert!(saved(2));
    term.wait();
}

/// A window can hang up before it closes, or without closing at all, with
/// `vibe` blocked writing to a terminal nobody reads: the hangup is still heard,
/// and saves. The terminal stays open until the save is seen, so nothing but the
/// `SIGHUP` can have told `vibe`.
#[test]
fn a_hangup_on_a_terminal_not_reading_saves_the_toolbox() {
    let w = toolbox_world();
    let mut cmd = w.vibe();
    cmd.arg("toolbox");
    let mut term = Term::spawn(cmd, Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("while :; do echo spam; done");
    term.expect("spam\r\nspam");
    term.stop_reading();
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
    let template = w.layout.linux_dir().join("template-1.img");
    wait_until("the save", || template.exists());
    // Only now: a Mac's exit waits for a terminal to drain.
    term.close_window();
    assert_eq!(term.wait(), Some(0));
    let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.starts_with("state     closed\n"), "{text}");
}

/// The shim gives up on a terminal that isn't reading, and then a resize fails to
/// reach it: a hangup after that is heard all the same.
#[test]
fn a_hangup_after_the_shim_gave_up_on_the_terminal_saves_the_toolbox() {
    let w = toolbox_world();
    let mut cmd = w.vibe();
    cmd.arg("toolbox");
    let mut term = Term::spawn(cmd, Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("while :; do echo spam; done");
    term.expect("spam\r\nspam");
    term.stop_reading();
    // Past the 30 s a shim gives a client that stops reading.
    std::thread::sleep(Duration::from_secs(33));
    // The first is taken, as the shim's reading end goes only on its next frame.
    term.resize(Size { rows: 25, cols: 81 });
    std::thread::sleep(Duration::from_millis(500));
    term.resize(Size { rows: 26, cols: 82 });
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
    let template = w.layout.linux_dir().join("template-1.img");
    wait_until("the save", || template.exists());
    term.close_window();
    assert_eq!(term.wait(), Some(0));
}

/// A detach isn't a hangup: however long a slow terminal takes, `vibe` waits to put
/// it back as it found it, rather than leave it on the alternate screen.
#[test]
fn a_detach_waits_for_a_terminal_slow_to_read() {
    let w = world();
    let _shim = w.shell();
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("printf '\\033[?1049h'; while :; do echo spam; done");
    term.expect("spam\r\nspam");
    term.stop_reading();
    term.type_bytes(b"\x1c");
    // Longer than a hangup would wait for it.
    std::thread::sleep(Duration::from_secs(4));
    term.start_reading();
    assert_eq!(term.wait(), Some(0));
    // Together, as the redraw on attaching says the first of these as well.
    term.expect("\x1b[?1049l\r\n[detached from quiet-amber-otter]");
}

/// An agent's window closing for real, with output running, is a detach:
/// exit 0, and the agent goes on.
#[test]
fn a_real_close_with_output_running_detaches_from_an_agent() {
    let w = world();
    let shim = w.shell();
    let mut term = w.attach("quiet-amber-otter", Size { rows: 24, cols: 80 });
    term.attached();
    term.type_line("while :; do echo spam; done");
    term.expect("spam\r\nspam");
    term.close_window_while_writing();
    assert_eq!(term.wait(), Some(0));
    wait_until("the detach", || clients(&w) == 0);
    assert!(alive(shim.pid), "the session goes on");
}

/// Over SSH a hangup can be the network dropping: it detaches as ever, and
/// the shell stays open to come back to.
#[test]
fn a_hangup_over_ssh_leaves_the_toolbox_open() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    for (name, value) in [
        ("SSH_CONNECTION", "192.0.2.1 50000 192.0.2.2 22"),
        ("SSH_TTY", "/dev/ttys042"),
    ] {
        let mut cmd = w.vibe();
        cmd.arg("toolbox").env(name, value);
        let mut term = Term::spawn(cmd, size);
        term.expect("Ctrl-\\ or a dropped connection leaves it open");
        term.attached();
        // SAFETY: a signal to the process this test started.
        unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
        assert_eq!(term.wait(), Some(0), "{}", term.text());
        assert!(
            !w.layout.linux_dir().join("template-1.img").exists(),
            "{name}"
        );
        let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(text.starts_with("state     open"), "{name}: {text}");
    }
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}

/// Closing the window during the boot closes what it was booting, rather
/// than leave a VM open with nothing attached (#54).
#[test]
fn a_hangup_during_the_boot_saves_the_toolbox() {
    let w = toolbox_world_with(|t| t.with_slow_open(Duration::from_secs(1)));
    let mut cmd = w.vibe();
    cmd.arg("toolbox");
    let mut term = Term::spawn(cmd, Size { rows: 24, cols: 80 });
    term.expect("Opening the Linux toolbox");
    // SAFETY: a signal to the process this test started.
    unsafe { libc::kill(term.child.id() as libc::pid_t, libc::SIGHUP) };
    assert_eq!(term.wait(), Some(0), "{}", term.text());
    let out = w.vibe().args(["toolbox", "status"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(text.starts_with("state     closed\n"), "{text}");
}

/// `vibe toolbox close` with a failed save behind it doesn't say there's
/// nothing to save: the changes are kept, unsaved.
#[test]
fn closing_after_a_failed_save_says_it_failed() {
    let w = toolbox_world();
    // Opened with no terminal: the answer's first byte says it's up.
    let mut open = w
        .vibe()
        .args(["rpc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        open.stdin.as_mut().unwrap(),
        r#"{{"jsonrpc":"2.0","id":1,"method":"toolbox.open","params":{{}}}}"#
    )
    .unwrap();
    let mut reply = [0u8; 1];
    open.stdout
        .as_mut()
        .unwrap()
        .read_exact(&mut reply)
        .unwrap();
    let _ = open.kill();
    let _ = open.wait();
    // The test toolbox can't save with no base image.
    std::fs::remove_dir_all(w.layout.linux_dir().join("base-1")).unwrap();
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("its last save failed"),
        "{out:?}"
    );
}

/// A shim too busy to answer is listed with no clients, which isn't
/// "nobody": a window closing then waits to find out, rather than end the
/// shell under another window once the shim answers vibed again (#54).
#[test]
fn a_shim_that_doesnt_answer_isnt_taken_for_no_terminals() {
    let w = toolbox_world();
    let size = Size { rows: 24, cols: 80 };
    let toolbox = || {
        let mut cmd = w.vibe();
        cmd.arg("toolbox");
        Term::spawn(cmd, size)
    };
    let mut first = toolbox();
    first.attached();
    let mut second = toolbox();
    second.attached();
    let socket = w.layout.session_socket("toolbox");
    let shim_pid = shim::connect(&socket, &control_hello("t"), WAIT)
        .unwrap()
        .status()
        .unwrap()
        .pid as libc::pid_t;
    // SAFETY: signals to a shim and a process this test started.
    unsafe {
        libc::kill(shim_pid, libc::SIGSTOP);
        libc::kill(first.child.id() as libc::pid_t, libc::SIGHUP);
    }
    // Long enough for a probe of it to give up: 5 s.
    std::thread::sleep(Duration::from_secs(7));
    // SAFETY: as above.
    unsafe { libc::kill(shim_pid, libc::SIGCONT) };
    assert_eq!(first.wait(), Some(0), "{}", first.text());
    assert!(
        !w.layout.linux_dir().join("template-1.img").exists(),
        "saved under the other window"
    );
    second.type_line("echo $((6*7))-still-here");
    second.expect("42-still-here");
    let out = w.vibe().args(["toolbox", "close"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
}
