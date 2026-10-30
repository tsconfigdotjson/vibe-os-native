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
use std::sync::{Arc, Mutex};
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
    _guard: SocketGuard,
    _rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
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
            .env("HOME", self.layout.home());
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
    master: File,
    /// Kept open, as a terminal emulator would.
    _slave: OwnedFd,
    child: Child,
    seen: Arc<Mutex<Vec<u8>>>,
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
        let master = File::from(master);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut reader = master.try_clone().unwrap();
        let sink = seen.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Term {
            master,
            _slave: slave,
            child,
            seen,
        }
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
        self.master.write_all(bytes).unwrap();
    }

    fn type_line(&mut self, line: &str) {
        self.type_bytes(format!("{line}\r").as_bytes());
    }

    fn resize(&self, size: Size) {
        set_size(self.master.as_raw_fd(), size);
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
            check(libc::tcgetattr(self.master.as_raw_fd(), &mut t)).unwrap();
            t.c_lflag & libc::ICANON != 0 && t.c_lflag & libc::ECHO != 0
        }
    }
}

impl Drop for Term {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
