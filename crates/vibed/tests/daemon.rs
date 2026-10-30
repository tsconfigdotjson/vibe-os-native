//! The real `vibed` binary: starting, serving, refusing a second instance and
//! stopping cleanly.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const VIBED: &str = env!("CARGO_BIN_EXE_vibed");

struct Daemon {
    child: Child,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(cmd: &mut Command, socket: &Path) -> Daemon {
    let child = cmd.stderr(Stdio::piped()).spawn().unwrap();
    let mut daemon = Daemon { child };
    let deadline = Instant::now() + Duration::from_secs(20);
    while UnixStream::connect(socket).is_err() {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            panic!("vibed exited early: {status}");
        }
        assert!(Instant::now() < deadline, "vibed didn't start");
        std::thread::sleep(Duration::from_millis(20));
    }
    daemon
}

fn hello(socket: &Path) -> Value {
    let mut s = UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"daemon.hello\",\"params\":{\"client\":\"daemon-test\",\"protocol\":1}}\n",
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

fn sigterm(child: &Child) {
    let ok = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap()
        .success();
    assert!(ok);
}

fn socket_in(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("run/vibed.sock")
}

#[test]
fn serves_hello_and_stops_cleanly_on_sigterm() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let mut d = spawn(Command::new(VIBED).arg("--socket").arg(&socket), &socket);
    let r = hello(&socket);
    let result = &r["result"];
    assert_eq!(result["daemon"], "vibed");
    assert_eq!(result["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(result["protocol"], 1);
    assert_eq!(result["os"], std::env::consts::OS);
    assert_eq!(result["arch"], std::env::consts::ARCH);
    assert_eq!(result["pid"], d.child.id());

    sigterm(&d.child);
    let out = d.child.wait().unwrap();
    assert!(out.success(), "{out}");
    assert!(!socket.exists(), "the socket is removed on exit");
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut d.child.stderr.take().unwrap(), &mut stderr).unwrap();
    assert!(stderr.contains("listening on"), "{stderr}");
    assert!(stderr.contains("hello from daemon-test"), "{stderr}");
    assert!(stderr.contains("vibed: stopped"), "{stderr}");
}

#[test]
fn a_second_daemon_on_the_same_socket_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let _first = spawn(Command::new(VIBED).arg("--socket").arg(&socket), &socket);
    let out = Command::new(VIBED)
        .arg("--socket")
        .arg(&socket)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("already serving"), "{stderr}");
    assert_eq!(
        hello(&socket)["result"]["daemon"],
        "vibed",
        "the first keeps serving"
    );
}

#[test]
fn socket_comes_from_vibe_socket() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = socket_in(&tmp);
    let _d = spawn(Command::new(VIBED).env("VIBE_SOCKET", &socket), &socket);
    assert_eq!(hello(&socket)["result"]["daemon"], "vibed");
}

#[test]
fn default_socket_is_under_home() {
    // Under /tmp: $TMPDIR plus the macOS default path would pass the 104-byte
    // limit on Unix socket paths.
    let tmp = tempfile::tempdir_in("/tmp").unwrap();
    let rel = vibe_core::paths::default_socket_relative(std::env::consts::OS);
    let socket = tmp.path().join(rel);
    let _d = spawn(
        Command::new(VIBED)
            .env_remove("VIBE_SOCKET")
            .env("HOME", tmp.path()),
        &socket,
    );
    assert_eq!(hello(&socket)["result"]["daemon"], "vibed");
}

#[test]
fn version_and_bad_args() {
    let out = Command::new(VIBED).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("vibed {}", env!("CARGO_PKG_VERSION"))
    );
    let out = Command::new(VIBED).arg("--bogus").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}
