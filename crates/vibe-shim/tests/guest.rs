//! `vibe-shim --vm`: a `linux` agent's terminal, held by the real
//! `vibe-guest` (run with `--vsock-dir`, so its ports are Unix sockets)
//! behind a stand-in for `vibe-vm` that splices each `connect` to them, as
//! `vibe-vm` splices it to vsock. The guest is the binary `cargo build`
//! puts beside this package's own; `cargo test --workspace` builds both.
//! The same path through a real VM is `vibe-env`'s `vm_cycle` test, on a
//! Mac.
#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vibe_core::rpc::SessionExit;
use vibe_core::shim::{
    self, Frame, FrameKind, Hello, PROTOCOL, Record, Reply, Request, Role, Size, control_hello,
};
use vibe_env::guest::{self as driver, GuestState, Launch, TermSize};

const SHIM: &str = env!("CARGO_BIN_EXE_vibe-shim");
const WAIT: Duration = Duration::from_secs(20);

fn guest_binary() -> PathBuf {
    let path = Path::new(SHIM).with_file_name("vibe-guest");
    assert!(
        path.exists(),
        "{} is missing: build it with `cargo build -p vibe-guest`",
        path.display()
    );
    path
}

fn tmp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("shimvm")
        .tempdir_in("/tmp")
        .unwrap()
}

fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// `vibe-guest` and a stand-in for the `vibe-vm` in front of it.
struct Vm {
    dir: tempfile::TempDir,
    guest: Child,
    socket: PathBuf,
    /// Every spliced stream, to cut them all as a VM going to sleep would.
    streams: Arc<Mutex<Vec<UnixStream>>>,
    _lock: File,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.guest.kill();
        let _ = self.guest.wait();
    }
}

impl Vm {
    fn start() -> Vm {
        let dir = tmp();
        let ports = dir.path().join("ports");
        std::fs::create_dir(&ports).unwrap();
        let guest = Command::new(guest_binary())
            .arg("--vsock-dir")
            .arg(&ports)
            .env("PS1", "$ ")
            .env("ENV", "/dev/null")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(File::create(dir.path().join("guest.log")).unwrap())
            .spawn()
            .unwrap();
        let socket = dir.path().join("r.vm");
        let lock = File::create(dir.path().join("r.vm.lock")).unwrap();
        lock.lock().unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let streams = Arc::new(Mutex::new(Vec::new()));
        let kept = streams.clone();
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let (ports, kept) = (ports.clone(), kept.clone());
                std::thread::spawn(move || splice(client.unwrap(), &ports, &kept));
            }
        });
        let vm = Vm {
            dir,
            guest,
            socket,
            streams,
            _lock: lock,
        };
        driver::wait_ready(&vm.socket, WAIT).unwrap();
        vm
    }

    /// Ends every stream into the guest: what a shim sees when its VM is
    /// saved and `vibe-vm` exits.
    fn cut(&self) {
        for s in self.streams.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn launch_shell(&self) -> u32 {
        let launch = Launch {
            command: vec!["sh".into()],
            env: Default::default(),
            cwd: Some(self.dir.path().to_string_lossy().into_owned()),
            size: Some(TermSize {
                rows: 30,
                cols: 100,
            }),
            time_ms: None,
        };
        driver::launch(&self.socket, &launch).unwrap().pid
    }

    fn state(&self) -> GuestState {
        driver::status(&self.socket).unwrap().state
    }

    fn shim(&self, name: &str) -> Shim {
        self.try_shim(name, &[]).unwrap()
    }

    fn try_shim(&self, name: &str, extra: &[&str]) -> Result<Shim, String> {
        let out = Command::new(SHIM)
            .args(["--ref", "quiet-amber-otter", "--socket"])
            .arg(self.dir.path().join(format!("run/{name}.sock")))
            .arg("--state")
            .arg(self.dir.path().join("agent"))
            .arg("--vm")
            .arg(&self.socket)
            .args(extra)
            .output()
            .unwrap();
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(Shim {
            pid: String::from_utf8(out.stdout)
                .unwrap()
                .trim()
                .parse()
                .unwrap(),
            socket: self.dir.path().join(format!("run/{name}.sock")),
            state: self.dir.path().join("agent"),
        })
    }
}

/// What `vibe-vm` does with a `connect`: answers it, then copies bytes
/// both ways, passing half-closes on.
fn splice(mut client: UnixStream, ports: &Path, kept: &Mutex<Vec<UnixStream>>) {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while client.read(&mut byte).unwrap_or(0) == 1 && byte[0] != b'\n' {
        line.push(byte[0]);
    }
    let request: serde_json::Value = serde_json::from_slice(&line).unwrap();
    let port = request["port"].as_u64().unwrap();
    let Ok(guest) = UnixStream::connect(ports.join(port.to_string())) else {
        let _ = client.write_all(
            b"{\"ok\":false,\"code\":\"connect_failed\",\"error\":\"nothing on that port\"}\n",
        );
        return;
    };
    client.write_all(b"{\"ok\":true}\n").unwrap();
    kept.lock().unwrap().push(client.try_clone().unwrap());
    kept.lock().unwrap().push(guest.try_clone().unwrap());
    let (mut c2, mut g2) = (client.try_clone().unwrap(), guest.try_clone().unwrap());
    let up = std::thread::spawn(move || {
        let _ = io::copy(&mut c2, &mut g2);
        let _ = g2.shutdown(Shutdown::Write);
    });
    let (mut g, mut c) = (guest, client);
    let _ = io::copy(&mut g, &mut c);
    let _ = c.shutdown(Shutdown::Write);
    let _ = up.join();
}

struct Shim {
    pid: i32,
    socket: PathBuf,
    state: PathBuf,
}

impl Drop for Shim {
    fn drop(&mut self) {
        if alive(self.pid) {
            // SAFETY: a signal to the shim this test started.
            unsafe { libc::kill(self.pid, libc::SIGKILL) };
        }
    }
}

impl Shim {
    fn attach(&self, size: Option<Size>) -> Attach {
        let hello = Hello {
            protocol: PROTOCOL,
            role: Role::Attach,
            client: "test".into(),
            tty: None,
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
        }
    }

    fn status(&self) -> shim::Status {
        let mut conn = shim::connect(&self.socket, &control_hello("test"), WAIT).unwrap();
        shim::write_frame(
            &mut conn.stream,
            &Frame::json(FrameKind::Request, &Request::Status),
        )
        .unwrap();
        match shim::read_frame(&mut conn.stream)
            .unwrap()
            .unwrap()
            .decode()
            .unwrap()
        {
            Reply::Status(s) => s,
            other => panic!("{other:?}"),
        }
    }

    fn signal(&self, signal: i32) {
        // SAFETY: a signal to the shim this test started.
        unsafe { libc::kill(self.pid, signal) };
    }

    fn wait_gone(&self) {
        wait_until("the shim to exit", || !alive(self.pid));
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.state.join(shim::LOG_FILE)).unwrap_or_default()
    }

    fn record(&self) -> Option<Record> {
        let text = std::fs::read_to_string(self.state.join(shim::RECORD_FILE)).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }
}

struct Attach {
    stream: UnixStream,
    output: String,
    exit: Option<SessionExit>,
}

impl Attach {
    fn type_line(&mut self, line: &str) {
        let frame = Frame::new(FrameKind::Input, format!("{line}\r").into_bytes());
        shim::write_frame(&mut self.stream, &frame).unwrap();
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        let frame = Frame::json(FrameKind::Resize, &Size { rows, cols });
        shim::write_frame(&mut self.stream, &frame).unwrap();
    }

    /// Reads one frame if there is one. False once the shim has closed.
    fn pump(&mut self) -> bool {
        match shim::read_frame(&mut self.stream) {
            Ok(Some(frame)) => {
                match frame.kind {
                    FrameKind::Output => self
                        .output
                        .push_str(&String::from_utf8_lossy(&frame.payload)),
                    FrameKind::Exit => self.exit = Some(frame.decode().unwrap()),
                    other => panic!("an attach client got a {other:?} frame"),
                }
                true
            }
            Ok(None) => false,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                true
            }
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => false,
            Err(e) => panic!("{e}"),
        }
    }

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

    fn expect_close(&mut self) {
        let deadline = Instant::now() + WAIT;
        while self.pump() {
            assert!(
                Instant::now() < deadline,
                "the shim didn't close: {:?}",
                self.output
            );
        }
    }
}

#[test]
fn a_linux_agents_terminal_is_held_in_the_guest_and_outlives_its_shims() {
    let vm = Vm::start();
    let pid = vm.launch_shell();

    // The shim takes the guest's size, as nothing says otherwise.
    let first = vm.shim("first");
    let status = first.status();
    assert_eq!(
        (status.child_pid, status.command, status.size),
        (
            pid,
            vec!["sh".to_owned()],
            Size {
                rows: 30,
                cols: 100
            }
        )
    );
    let mut a = first.attach(None);
    a.type_line("stty size; echo one $((1 + 1))");
    a.expect("30 100");
    a.expect("one 2");
    // A client's size is passed on to the guest's terminal.
    a.resize(20, 70);
    a.type_line("stty size");
    a.expect("20 70");

    // The shim dies: the command goes on, and the next shim shows the
    // screen as the guest has it.
    first.signal(libc::SIGKILL);
    first.wait_gone();
    assert_eq!(vm.state(), GuestState::Running);
    let second = vm.shim("second");
    let mut b = second.attach(None);
    b.expect("one 2");
    b.type_line("echo two $((1 + 1 + 1))");
    b.expect("two 3");

    // Asked to stop, a guest's shim leaves the command running, with no
    // record of an end and nothing signalled on this host.
    second.signal(libc::SIGTERM);
    second.wait_gone();
    b.expect_close();
    assert!(b.exit.is_none());
    assert!(second.record().is_none(), "{:?}", second.record());
    assert!(!second.socket.exists());
    assert!(
        second.log().contains("goes on in the guest"),
        "{}",
        second.log()
    );
    assert_eq!(vm.state(), GuestState::Running);

    // The VM goes to sleep: the stream drops, and the shim exits the same
    // way.
    let third = vm.shim("third");
    let mut c = third.attach(None);
    c.expect("two 3");
    vm.cut();
    third.wait_gone();
    c.expect_close();
    assert!(third.record().is_none());
    assert_eq!(vm.state(), GuestState::Running);

    // The command ends: this time there's a record, as for a PTY.
    let fourth = vm.shim("fourth");
    let mut d = fourth.attach(None);
    d.type_line("exit 4");
    d.expect_close();
    assert_eq!(d.exit.as_ref().and_then(|e| e.code), Some(4));
    fourth.wait_gone();
    let record = fourth.record().expect("a record of the end");
    assert_eq!(
        (record.exit.code, record.command),
        (Some(4), vec!["sh".to_owned()])
    );
    assert!(
        record.output.screen.contains("two 3"),
        "{:?}",
        record.output
    );
    assert_eq!(vm.state(), GuestState::Exited);
    // A shim for a command that already ended reports the end and goes.
    std::fs::remove_file(fourth.state.join(shim::RECORD_FILE)).unwrap();
    let fifth = vm.shim("fifth");
    fifth.wait_gone();
    assert_eq!(fifth.record().map(|r| r.exit.code), Some(Some(4)));
}

#[test]
fn a_shim_with_nothing_to_attach_to_says_why() {
    let vm = Vm::start();
    let err = vm.try_shim("none", &[]).err().unwrap();
    assert!(err.contains("nothing is running in the guest yet"), "{err}");
    // An explicit size is the terminal's from the start.
    vm.launch_shell();
    let shim = vm.try_shim("sized", &["--size", "90x25"]).unwrap();
    assert_eq!(shim.status().size, Size { rows: 25, cols: 90 });
    let mut a = shim.attach(None);
    a.type_line("stty size");
    a.expect("25 90");
}
