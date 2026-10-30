//! The `vibe` binary against an in-process `vibed`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;

use serde_json::Value;
use vibe_core::rpc::MAX_MESSAGE_BYTES;
use vibed::dispatch::DaemonInfo;
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
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("run/vibed.sock");
    let (listener, guard) = server::bind(&path).unwrap();
    rt.spawn(async move {
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        server::serve(
            listener,
            Arc::new(info),
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
    let s = start(DaemonInfo {
        version: "0.1.0".into(),
        os: "macos".into(),
        arch: "aarch64".into(),
        pid: 4242,
    });
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../protocol/examples");
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    files.sort();
    let mut input = Vec::new();
    let mut expected = Vec::new();
    for f in files {
        let ex: Value = serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap();
        match ex.get("request_raw") {
            Some(raw) => input.extend_from_slice(raw.as_str().unwrap().as_bytes()),
            None => input.extend(serde_json::to_vec(&ex["request"]).unwrap()),
        }
        input.push(b'\n');
        if !ex["response"].is_null() {
            expected.push(ex["response"].clone());
        }
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
