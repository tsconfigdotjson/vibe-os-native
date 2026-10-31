//! The `vibe` binary against an in-process `vibed`.

// vibed's test helpers: the golden world the contract examples need.
#[path = "../../vibed/tests/common/mod.rs"]
mod common;

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use serde_json::Value;
use vibe_core::paths::Layout;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::core::{Config, Core, DaemonInfo};
use vibed::server::{self, SocketGuard};

const VIBE: &str = env!("CARGO_BIN_EXE_vibe");

struct Server {
    path: PathBuf,
    _guard: SocketGuard,
    _dir: tempfile::TempDir,
    // Dropped last: stops the server.
    _rt: tokio::runtime::Runtime,
}

fn start(info: DaemonInfo) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let os = info.os.clone();
    let core = Core::open(Config::new(info, Layout::new(dir.path().join("home"), &os))).unwrap();
    start_core(core, dir)
}

/// Serves `core` on a socket in `dir`, which is kept until the server goes.
fn start_core(core: Core, dir: tempfile::TempDir) -> Server {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let path = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    rt.spawn(async move {
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        server::serve(
            listener,
            Arc::new(core),
            MAX_MESSAGE_BYTES,
            std::future::pending(),
        )
        .await;
    });
    Server {
        path,
        _guard: guard,
        _dir: dir,
        _rt: rt,
    }
}

fn vibe(args: &[&str], socket: &Path, stdin: &[u8]) -> Output {
    let mut child = Command::new(VIBE)
        .args(args)
        .env("VIBE_SOCKET", socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    child.wait_with_output().unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn hello_prints_what_vibed_reports() {
    let s = start(DaemonInfo::current());
    let out = vibe(&["hello"], &s.path, b"");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let expected = format!(
        "vibed {} · protocol 1 · {}/{} · pid {} · {}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::process::id(),
        s.path.display()
    );
    assert_eq!(text(&out.stdout), expected);
}

#[test]
fn socket_flag_beats_the_environment() {
    let s = start(DaemonInfo::current());
    let path = s.path.to_str().unwrap();
    let out = vibe(
        &["--socket", path, "hello"],
        Path::new("/nonexistent.sock"),
        b"",
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
}

#[test]
fn hello_without_a_daemon_fails_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("none.sock");
    for cmd in ["hello", "rpc"] {
        let out = vibe(&[cmd], &missing, b"");
        assert_eq!(out.status.code(), Some(1), "{cmd}");
        let stderr = text(&out.stderr);
        assert!(stderr.contains("can't reach vibed at"), "{cmd}: {stderr}");
        assert!(stderr.contains("is vibed running?"), "{cmd}: {stderr}");
    }
}

#[test]
fn rpc_bridges_the_whole_contract() {
    // The golden daemon of protocol/examples, so every reply is exact.
    let golden = common::Golden::new().for_examples();
    let core = Core::open(golden.config()).unwrap();
    let s = start_core(core, tempfile::tempdir().unwrap());
    let mut input = Vec::new();
    let mut expected = Vec::new();
    for ex in common::examples(&golden.home) {
        let Some(line) = ex.line else { continue };
        input.extend(line);
        input.push(b'\n');
        if !ex.response.is_null() {
            expected.push(ex.response);
        }
        expected.extend(ex.events);
    }
    let out = vibe(&["rpc"], &s.path, &input);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let got: Vec<Value> = text(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(got, expected);
}

#[test]
fn version_and_usage() {
    let out = Command::new(VIBE).arg("--version").output().unwrap();
    assert_eq!(
        text(&out.stdout),
        format!("vibe {}\n", env!("CARGO_PKG_VERSION"))
    );
    let out = Command::new(VIBE).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out.stderr).contains("usage: vibe"));
}

const TOKEN: &str = "sk-ant-oat01-a-test-token-from-a-stand-in-for-claude-0123456789";

/// `vibe setup-token` with a stand-in for `claude` that prints `body`.
fn setup_token(dir: &Path, body: &str) -> (Output, PathBuf) {
    let claude = dir.join("claude");
    fs::write(&claude, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let token = dir.join("token");
    let out = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", &claude)
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (out, token)
}

#[test]
fn setup_token_keeps_the_token_claude_printed() {
    let tmp = tempfile::tempdir().unwrap();
    // Run on a terminal, with escapes around the token, as Claude's TUI
    // prints it; and asked for nothing but `setup-token`.
    let (out, token) = setup_token(
        tmp.path(),
        &format!(
            "[ \"$1\" = setup-token ] || exit 9\n\
             [ -t 1 ] || {{ echo 'not a terminal'; exit 8; }}\n\
             printf '\\033[1mYour OAuth token (valid for 1 year):\\033[22m\\r\\n\\r\\n\\033[32m{TOKEN}\\033[39m\\r\\n'"
        ),
    );
    assert!(
        out.status.success(),
        "{}{}",
        text(&out.stdout),
        text(&out.stderr)
    );
    assert!(
        text(&out.stdout).contains("Your OAuth token"),
        "it's shown as it runs"
    );
    assert_eq!(fs::read_to_string(&token).unwrap().trim(), TOKEN);
    let mode = fs::metadata(&token).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn setup_token_without_a_token_fails_and_keeps_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (out, token) = setup_token(tmp.path(), "echo 'Login cancelled'");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("printed no token"),
        "{}",
        text(&out.stderr)
    );
    assert!(!token.exists());
    let (out, token) = setup_token(tmp.path(), "echo 'error'; exit 3");
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("didn't finish"),
        "{}",
        text(&out.stderr)
    );
    assert!(!token.exists());
    // No claude at all.
    let out = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", tmp.path().join("missing"))
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("can't run"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn setup_token_asks_for_a_paste_on_a_terminal_when_it_saw_none() {
    use std::io::Read;
    use std::os::fd::{FromRawFd, OwnedFd};
    let tmp = tempfile::tempdir().unwrap();
    let claude = tmp.path().join("claude");
    fs::write(&claude, "#!/bin/sh\necho 'Opened your browser.'\n").unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
    let token = tmp.path().join("token");
    // A terminal for vibe's stdin, stdout and stderr, as a person's is.
    // SAFETY: each call is checked; the descriptors are owned at once.
    let (master, slave) = unsafe {
        let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(m >= 0);
        assert_eq!(libc::grantpt(m), 0);
        assert_eq!(libc::unlockpt(m), 0);
        let name = std::ffi::CStr::from_ptr(libc::ptsname(m)).to_owned();
        let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
        assert!(s >= 0);
        (OwnedFd::from_raw_fd(m), OwnedFd::from_raw_fd(s))
    };
    let mut child = Command::new(VIBE)
        .arg("setup-token")
        .env("VIBE_CLAUDE", &claude)
        .env("VIBE_TEST_TOKEN_FILE", &token)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()))
        .spawn()
        .unwrap();
    drop(slave);
    let mut master = fs::File::from(master);
    let mut reader = master.try_clone().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            let _ = tx.send(String::from_utf8_lossy(&seen).into_owned());
        }
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let seen = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("vibe asked for nothing");
        if seen.contains("paste the token") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{seen}");
    }
    master.write_all(format!("{TOKEN}\n").as_bytes()).unwrap();
    let status = child.wait().unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(&token).unwrap().trim(), TOKEN);
}
